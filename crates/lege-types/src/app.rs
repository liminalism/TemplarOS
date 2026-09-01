//! Canonical application identity, instances and manifests (planning doc 02 §2).

use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, path::PathBuf, str::FromStr};
use thiserror::Error;
use uuid::Uuid;

/// Canonical, registry-controlled application identifier in reverse-DNS form
/// (for example `org.legeos.Files`).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppId(String);

/// Why a string is not a valid [`AppId`].
#[derive(Debug, Error, Eq, PartialEq)]
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
    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Identifier of one running application instance (one launch, one cgroup, N windows).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(Uuid);

impl InstanceId {
    /// A fresh, unguessable instance identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// The 32-hex-digit form used inside systemd unit names.
    pub fn compact(&self) -> String {
        self.0.simple().to_string()
    }

    /// Parse either the hyphenated or the compact form.
    pub fn parse(text: &str) -> Option<Self> {
        Uuid::parse_str(text).ok().map(Self)
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

/// Compatibility identities that map onto one canonical application.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppAliases {
    /// Names such as `org.example.Editor.desktop`.
    #[serde(default)]
    pub desktop_file_ids: BTreeSet<String>,

    /// Values supplied through `xdg_toplevel.set_app_id`.
    #[serde(default)]
    pub wayland_app_ids: BTreeSet<String>,

    /// XWayland `WM_CLASS` / desktop `StartupWMClass` values.
    #[serde(default)]
    pub startup_wm_classes: BTreeSet<String>,

    /// Flatpak application IDs.
    #[serde(default)]
    pub flatpak_ids: BTreeSet<String>,

    /// Compatibility evidence only. A path alone is not the root of trust.
    #[serde(default)]
    pub executable_paths: BTreeSet<PathBuf>,
}

/// Static resource classification declared by a manifest; the governor refines it at runtime.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResourceClass {
    /// Compositor, input, audio, recovery UI: never an application victim.
    SessionCritical,
    /// Editors, terminals, browsers: strongly protected while foreground.
    Interactive,
    /// Ordinary applications.
    Normal,
    /// Sync agents, indexers, background helpers.
    Background,
    /// Trivially recreated (a clock, a calculator): first to go under pressure.
    Disposable,
}

/// The application contract as installed on the system.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppManifest {
    pub id: AppId,
    pub display_name: String,

    /// Compatibility ID exported to `mimeapps.list` and desktop menus.
    pub primary_desktop_file_id: String,

    /// Exact executable used when the launcher starts the application.
    pub executable: PathBuf,

    #[serde(default)]
    pub aliases: AppAliases,

    /// Exact MIME types or broad entries such as `image/*`.
    #[serde(default)]
    pub mime_types: BTreeSet<String>,

    pub resource_class: ResourceClass,

    /// Whether the application implements the checkpoint protocol (see [`crate::checkpoint`]).
    #[serde(default)]
    pub supports_checkpoint: bool,

    /// The `Icon=` value: an icon-theme name (`firefox`) or an absolute path. Carried here so a
    /// launcher does not have to re-find and re-parse the `.desktop` file the registry already
    /// read — and so an application with no desktop file at all can still declare one.
    #[serde(default)]
    pub icon: Option<String>,

    /// freedesktop `Categories=` (`Network`, `WebBrowser`, …). Not a taxonomy LegeOS enforces:
    /// they exist so a launcher can match "browser" against something.
    #[serde(default)]
    pub categories: BTreeSet<String>,

    /// freedesktop `Keywords=`: the search terms the application itself suggests.
    #[serde(default)]
    pub keywords: BTreeSet<String>,

    /// `NoDisplay=true`: installed, launchable and resolvable by MIME association, but not to be
    /// *offered* in a launcher or menu. Helpers, MIME shims and autostart shells set it, and a
    /// launcher listing that ignores it drowns in entries no person ever means to click.
    #[serde(default)]
    pub no_display: bool,

    /// Where this copy came from (see [`crate::variant`]). Every installed `AppId` names one
    /// *variant* — one copy, from one origin — and this is what is known about that origin.
    /// `None` on a manifest written before provenance existed, or hand-authored without it; that
    /// reads as an unknown origin, and an unknown origin is untrusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<crate::variant::AppProvenance>,
}

/// What a client *claims* about itself. Claims are checked, never trusted.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowClaims {
    pub wayland_app_id: Option<String>,
    pub startup_wm_class: Option<String>,
    pub sandbox_app_id: Option<String>,
}

impl AppManifest {
    /// How far this copy may be trusted. A manifest with no provenance has an unknown origin, and
    /// an unknown origin is untrusted — the same answer a hostile file would get, which is the
    /// point.
    pub fn trust(&self) -> crate::variant::TrustTier {
        self.provenance.as_ref().map_or(
            crate::variant::TrustTier::User,
            crate::variant::AppProvenance::effective_trust,
        )
    }

    /// The logical application this copy *declares* it belongs to, if it declares one.
    pub fn declared_logical_app(&self) -> Option<&crate::variant::LogicalAppId> {
        self.provenance.as_ref()?.logical.as_ref()
    }

    /// Checks whether at least one supplied compatibility claim belongs to this canonical
    /// application. This does not prove identity — the registered launch cgroup does.
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

    /// Whether the manifest declares support for `mime`, directly or through a `major/*` entry.
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

/// Read access to installed manifests. Implemented by the application registry; consumed by
/// the association engine, the launcher, the shell and `lege doctor`.
pub trait AppCatalog: Send + Sync {
    /// The manifest for `id`, if installed.
    fn manifest(&self, id: &AppId) -> Option<std::sync::Arc<AppManifest>>;

    /// Every installed manifest (order unspecified).
    fn manifests(&self) -> Vec<std::sync::Arc<AppManifest>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_validation() {
        assert!("org.legeos.Files".parse::<AppId>().is_ok());
        assert_eq!("files".parse::<AppId>(), Err(AppIdError::InvalidSegments));
        assert_eq!(
            "org..Files".parse::<AppId>(),
            Err(AppIdError::InvalidSegments)
        );
        assert_eq!(
            "org.lege os".parse::<AppId>(),
            Err(AppIdError::InvalidCharacter)
        );
        assert_eq!("a.".parse::<AppId>(), Err(AppIdError::InvalidLength));
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = AppManifest {
            id: "org.legeos.Viewer".parse().unwrap(),
            display_name: "Lege Viewer".into(),
            primary_desktop_file_id: "org.legeos.Viewer.desktop".into(),
            executable: "/usr/bin/lege-viewer".into(),
            aliases: AppAliases {
                wayland_app_ids: ["org.legeos.Viewer".to_owned()].into(),
                ..AppAliases::default()
            },
            mime_types: ["application/pdf".to_owned(), "image/*".to_owned()].into(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: true,
            icon: Some("lege-viewer".into()),
            categories: ["Graphics".to_owned(), "Viewer".to_owned()].into(),
            keywords: ["pdf".to_owned()].into(),
            no_display: false,
            provenance: None,
        };

        let json = serde_json::to_string(&manifest).unwrap();
        let back: AppManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, manifest);
        assert!(back.supports_mime("image/tiff"));
        assert!(back.supports_mime("application/pdf"));
        assert!(!back.supports_mime("text/plain"));
    }

    #[test]
    fn a_manifest_written_before_the_launcher_fields_existed_still_reads() {
        // The registry persists manifests as JSON; an installed system upgraded into these
        // fields must not fail to parse its own store.
        let json = r#"{
            "id": "org.legeos.Viewer",
            "display_name": "Lege Viewer",
            "primary_desktop_file_id": "org.legeos.Viewer.desktop",
            "executable": "/usr/bin/lege-viewer",
            "resource_class": "Interactive"
        }"#;
        let manifest: AppManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.icon, None);
        assert!(manifest.categories.is_empty());
        assert!(manifest.keywords.is_empty());
        assert!(!manifest.no_display, "absent NoDisplay means displayable");
        assert!(manifest.provenance.is_none());
        assert_eq!(
            manifest.trust(),
            crate::variant::TrustTier::User,
            "a manifest that says nothing about where it came from is not the system's"
        );
    }

    #[test]
    fn wm_class_claims_are_case_insensitive() {
        let manifest = AppManifest {
            id: "org.legeos.Viewer".parse().unwrap(),
            display_name: String::new(),
            primary_desktop_file_id: "org.legeos.Viewer.desktop".into(),
            executable: PathBuf::new(),
            aliases: AppAliases {
                startup_wm_classes: ["lege-viewer".to_owned()].into(),
                ..AppAliases::default()
            },
            mime_types: BTreeSet::new(),
            resource_class: ResourceClass::Normal,
            supports_checkpoint: false,
            icon: None,
            categories: BTreeSet::new(),
            keywords: BTreeSet::new(),
            no_display: false,
            provenance: None,
        };

        let claims = WindowClaims {
            startup_wm_class: Some("Lege-Viewer".into()),
            ..WindowClaims::default()
        };
        assert!(manifest.accepts_window_claims(&claims));
    }
}
