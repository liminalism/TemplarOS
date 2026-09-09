//! Where one installed copy of an application came from, and which application it is a copy *of*.
//!
//! An [`AppId`] names a **variant**: one installed copy, from one origin. A person sees one
//! Firefox; the system may hold the distribution's package, the Flathub build, and a `.desktop`
//! file they wrote themselves. [`AppVariantId`] is a name for that reading of an `AppId`;
//! [`LogicalAppId`] is the second, new identity — "Firefox", the thing a person means.
//!
//! Grouping variants under a logical application is **presentation**. Every right — MIME
//! defaults, provider declarations, autostart approvals, instances — stays keyed on the variant,
//! because a grant belongs to the copy that earned it. A wrong grouping therefore draws two
//! applications as one; it can never hand one copy's permissions to another.
//!
//! [`AppProvenance`] is likewise **inert**: `declared_privileges` and `provider_claims` are what a
//! copy's own metadata says about itself. A claim is a thing to display and to compare a grant
//! against, never a grant. The one conclusion drawn from provenance is [`TrustTier`], and it is
//! derived from the origin, never declared.

use crate::app::{AppId, AppIdError};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt, path::PathBuf, str::FromStr};

/// Today's [`AppId`] *is* the variant id. An alias rather than a newtype: it names the reading
/// without forcing a tree-wide rename, and the rename stays mechanically separable if it is ever
/// worth doing.
pub type AppVariantId = AppId;

/// The application a person means, across every installed copy of it.
///
/// Shares [`AppId`]'s validator — one grammar for both identities, so a logical id is a legal
/// application id and can be stored, logged and matched by the same code.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LogicalAppId(String);

impl FromStr for LogicalAppId {
    type Err = AppIdError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        AppId::from_str(raw).map(|id| Self(id.as_str().to_owned()))
    }
}

impl LogicalAppId {
    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogicalAppId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Which kind of place a copy came from. Not a taxonomy of vendors — a taxonomy of who is able to
/// write the file, which is the only part of an origin that can be reasoned about.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum AppOriginKind {
    /// A TemplarOS manifest under `/usr/share/templaros/apps` or `/etc/templaros/apps`.
    NativeManifest,
    /// A TemplarOS manifest under `$XDG_DATA_HOME/templaros/apps`: the person's own.
    NativeUserManifest,
    /// A `.desktop` entry from a system application directory, owned by a package.
    DistroPackage,
    /// A Flatpak export.
    Flatpak,
    /// A `.desktop` entry in the person's own data directory.
    UserDesktopEntry,
    /// Nothing could be concluded. Treated as untrusted, because a guess is not evidence.
    #[default]
    Unknown,
}

impl AppOriginKind {
    /// The trust tier this origin implies. Trust is *derived*: no application declares it.
    pub fn trust(self) -> TrustTier {
        match self {
            Self::NativeManifest | Self::DistroPackage | Self::Flatpak => TrustTier::System,
            Self::NativeUserManifest | Self::UserDesktopEntry | Self::Unknown => TrustTier::User,
        }
    }

    /// Who is expected to update a copy from this origin — the honest answer to "why is this
    /// still the old version".
    pub fn update_owner(self) -> UpdateOwner {
        match self {
            Self::NativeManifest | Self::DistroPackage => UpdateOwner::Distro,
            Self::Flatpak => UpdateOwner::Flatpak,
            Self::NativeUserManifest | Self::UserDesktopEntry => UpdateOwner::User,
            Self::Unknown => UpdateOwner::Unknown,
        }
    }

    /// The kebab-case name used on the wire and in the schema.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NativeManifest => "native-manifest",
            Self::NativeUserManifest => "native-user-manifest",
            Self::DistroPackage => "distro-package",
            Self::Flatpak => "flatpak",
            Self::UserDesktopEntry => "user-desktop-entry",
            Self::Unknown => "unknown",
        }
    }

    /// Decode the stored/wire form; anything unrecognised is [`AppOriginKind::Unknown`], which is
    /// the untrusted answer — a name a future version invents must not read as trusted here.
    pub fn from_str_or_unknown(value: &str) -> Self {
        match value {
            "native-manifest" => Self::NativeManifest,
            "native-user-manifest" => Self::NativeUserManifest,
            "distro-package" => Self::DistroPackage,
            "flatpak" => Self::Flatpak,
            "user-desktop-entry" => Self::UserDesktopEntry,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for AppOriginKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How far a copy may be trusted, derived from its origin and nothing else.
///
/// `System` means something with administrative rights over the machine owns the file — a package,
/// a system manifest, a Flatpak installation. `User` means a file the person's own account can
/// rewrite at will, which is exactly the file an attacker who reached that account would write.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum TrustTier {
    /// Owned by the system. May take an exclusive provider role.
    System,
    /// Owned by the person's account. May be launched and associated with files freely; may not
    /// take an exclusive provider role.
    #[default]
    User,
}

impl TrustTier {
    /// The kebab-case name used on the wire and in the schema.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }

    /// Decode the stored/wire form. Anything unrecognised is `User`: the safe reading.
    pub fn from_str_or_user(value: &str) -> Self {
        if value == "system" {
            Self::System
        } else {
            Self::User
        }
    }

    /// Whether a copy at this tier may take an exclusive provider role.
    pub fn may_hold_exclusive_role(self) -> bool {
        matches!(self, Self::System)
    }
}

impl fmt::Display for TrustTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How a copy is actually started.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchMechanism {
    /// An executable, run directly into its own unit.
    Direct,
    /// One of Flatpak's export wrappers — itself directly executable, never `flatpak run` through
    /// a shell.
    FlatpakExport,
    /// A script that starts something else. Recorded because it makes the executable path weak
    /// evidence for grouping.
    Wrapper,
    #[default]
    Unknown,
}

/// Who is expected to update a copy.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateOwner {
    Distro,
    Flatpak,
    User,
    #[default]
    Unknown,
}

/// Where one installed copy of an application came from.
///
/// Every field is optional or defaulted on read: a manifest written before provenance existed
/// still parses, and an origin that could not be determined is [`AppOriginKind::Unknown`] rather
/// than a guess. Mirrors `interfaces/schemas/app-variant.schema.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppProvenance {
    #[serde(default)]
    pub origin: AppOriginKind,

    /// Derived from `origin`; stored so a reader need not re-derive it, and audited against
    /// `origin.trust()` wherever it matters.
    #[serde(default)]
    pub trust: TrustTier,

    /// The `.desktop` file or manifest this copy was derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,

    /// The packaging system's own name for this copy: a Debian package name, a Flatpak ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_ref: Option<String>,

    /// The version the origin reports, verbatim. Displayed, never compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// The merged desktop-entry id this copy exports. Two copies sharing one, from different
    /// origins, is strong evidence they are the same application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_entry_id: Option<String>,

    #[serde(default)]
    pub launch: LaunchMechanism,

    /// The resolved executable. Evidence for grouping; never a root of trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,

    /// Whether the copy runs inside a sandbox TemplarOS did not build. Reported, not acted on.
    #[serde(default)]
    pub sandboxed: bool,

    #[serde(default)]
    pub update_owner: UpdateOwner,

    /// What the copy's own metadata says it needs. Inert: a claim is not a grant.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub declared_privileges: BTreeSet<String>,

    /// Exclusive roles the copy's metadata says it can fill. Also inert — `Providers1` decides,
    /// and it checks trust first.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub provider_claims: BTreeSet<String>,

    /// The logical application this copy *declares* it belongs to. Grouping otherwise comes from
    /// evidence rather than from a claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical: Option<LogicalAppId>,

    /// Unix milliseconds when this variant was first recorded. Stable across re-scans; it is what
    /// makes "the older copy" a meaningful phrase.
    #[serde(default)]
    pub first_seen_ms: i64,
}

impl AppProvenance {
    /// Provenance for a copy from `origin`, with trust and update owner derived from it.
    pub fn from_origin(origin: AppOriginKind) -> Self {
        Self {
            origin,
            trust: origin.trust(),
            update_owner: origin.update_owner(),
            ..Self::default()
        }
    }

    /// The trust tier to act on. Reads through to the origin rather than the stored field, so a
    /// hand-edited or hostile manifest cannot promote itself by writing `"trust": "system"`.
    pub fn effective_trust(&self) -> TrustTier {
        self.origin.trust()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_is_derived_from_the_origin_and_cannot_be_declared() {
        let mut provenance = AppProvenance::from_origin(AppOriginKind::UserDesktopEntry);
        assert_eq!(provenance.trust, TrustTier::User);
        // A manifest that writes its own trust in is still read as what its origin allows.
        provenance.trust = TrustTier::System;
        assert_eq!(provenance.effective_trust(), TrustTier::User);
        assert!(!provenance.effective_trust().may_hold_exclusive_role());
    }

    #[test]
    fn unknown_origins_read_as_untrusted() {
        assert_eq!(
            AppOriginKind::from_str_or_unknown("something-invented-later"),
            AppOriginKind::Unknown
        );
        assert_eq!(AppOriginKind::Unknown.trust(), TrustTier::User);
        assert_eq!(TrustTier::from_str_or_user("System"), TrustTier::User);
    }

    #[test]
    fn provenance_round_trips_and_an_empty_object_reads() {
        let provenance = AppProvenance {
            source_path: Some(PathBuf::from("/usr/share/applications/firefox.desktop")),
            package_ref: Some("firefox".into()),
            desktop_entry_id: Some("firefox.desktop".into()),
            launch: LaunchMechanism::Direct,
            executable: Some(PathBuf::from("/usr/bin/firefox")),
            provider_claims: ["web-browser".to_owned()].into(),
            logical: Some("logical.firefox".parse().unwrap()),
            first_seen_ms: 1_700_000_000_000,
            ..AppProvenance::from_origin(AppOriginKind::DistroPackage)
        };
        let json = serde_json::to_string(&provenance).unwrap();
        assert_eq!(
            serde_json::from_str::<AppProvenance>(&json).unwrap(),
            provenance
        );

        let empty: AppProvenance = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.origin, AppOriginKind::Unknown);
        assert_eq!(empty.effective_trust(), TrustTier::User);
        assert_eq!(empty.first_seen_ms, 0);
    }

    #[test]
    fn a_logical_id_is_a_legal_application_id() {
        assert!("logical.firefox".parse::<LogicalAppId>().is_ok());
        assert!("firefox".parse::<LogicalAppId>().is_err());
        assert_eq!(
            "logical.firefox".parse::<LogicalAppId>().unwrap().as_str(),
            "logical.firefox"
        );
    }

    #[test]
    fn update_owner_follows_who_installed_the_copy() {
        assert_eq!(AppOriginKind::Flatpak.update_owner(), UpdateOwner::Flatpak);
        assert_eq!(
            AppOriginKind::DistroPackage.update_owner(),
            UpdateOwner::Distro
        );
        assert_eq!(
            AppOriginKind::UserDesktopEntry.update_owner(),
            UpdateOwner::User
        );
    }
}
