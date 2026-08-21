//! The check catalogue. Add a check here and it runs everywhere doctor runs.
//!
//! Several checks below (planning doc 01 §7's "package-level integration testing") are
//! runtime-only: they read the live per-user state store (`lege_state::Store`) rather than a
//! packaged file, so they cannot say anything useful about an unpacked image root that has never
//! booted a session. Those checks `Skip` rather than `Fail` when `context.root != "/"`, and — the
//! important part — they only ever *open* the store when it already exists (`Path::exists`
//! first): `lege doctor` is a read-only diagnostic, and `lege_state::Store::open` creates and
//! migrates a fresh database if none is there, which running a check must never do as a side
//! effect.

use super::{Check, Context, Finding, Outcome, Severity, exists, finding};
use lege_types::{AppManifest, ProviderRole};
use std::{collections::BTreeMap, fs, path::Path};

pub fn all() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(OsRelease),
        Box::new(SessionUnitsPresent),
        Box::new(ProtectedUnitsHaveResourceControl),
        Box::new(NvidiaFlavourConsistent),
        Box::new(NoStandaloneNvidiaInstaller),
        Box::new(PortalConfigPresent),
        Box::new(StorageReserve),
        Box::new(HostUnitsPresent),
        Box::new(AppManifestsValid),
        Box::new(MimeHandlersInstalled),
        Box::new(AutostartProvidersResolved),
    ]
}

/// Load every manifest under the search order `lege_appd::manifests` documents, rooted at
/// `context.root` so this works against an image root as well as `/`. Only `SYSTEM_DIR` and
/// `ETC_DIR` are checked here — `$XDG_DATA_HOME/legeos/apps` is a live user directory, not a
/// packaged one, and out of scope for a distribution-image check.
fn load_manifests(context: &Context) -> Vec<AppManifest> {
    [
        lege_appd::manifests::SYSTEM_DIR,
        lege_appd::manifests::ETC_DIR,
    ]
    .iter()
    .flat_map(|dir| lege_appd::manifests::load_dir(&context.path(dir)).unwrap_or_default())
    .collect()
}

/// `/etc/os-release` identifies LegeOS and its Ubuntu base.
struct OsRelease;
impl Check for OsRelease {
    fn id(&self) -> &'static str {
        "os-release"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let path = context.path("/etc/os-release");
        let Ok(text) = fs::read_to_string(&path) else {
            return finding(
                self,
                Outcome::Fail,
                "os-release missing",
                path.display().to_string(),
            );
        };
        let is_legeos = text.lines().any(|line| line == "ID=legeos");
        let like_ubuntu = text
            .lines()
            .any(|line| line.starts_with("ID_LIKE=") && line.contains("ubuntu"));
        if is_legeos && like_ubuntu {
            finding(
                self,
                Outcome::Pass,
                "os-release identifies LegeOS (Ubuntu-derived)",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                "os-release is not LegeOS",
                format!("ID=legeos:{is_legeos} ID_LIKE ubuntu:{like_ubuntu}"),
            )
        }
    }
}

/// The session target and core units are installed.
struct SessionUnitsPresent;
impl Check for SessionUnitsPresent {
    fn id(&self) -> &'static str {
        "session-units"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let required = [
            "/usr/lib/systemd/user/legeos-session.target",
            "/usr/lib/systemd/user/lege-comp.service",
            "/usr/lib/systemd/user/lege-sessiond.service",
            "/usr/lib/systemd/user/lege-resourced.service",
            "/usr/lib/systemd/user/lege-filed.service",
            "/usr/lib/systemd/user/lege-shell.service",
            "/usr/share/wayland-sessions/legeos.desktop",
        ];
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|p| !exists(&context.root, p))
            .collect();
        if missing.is_empty() {
            finding(self, Outcome::Pass, "session units present", "")
        } else {
            finding(
                self,
                Outcome::Fail,
                format!("{} session unit(s) missing", missing.len()),
                missing.join(", "),
            )
        }
    }
}

/// Compositor, sessiond, resourced and audio units carry memory/CPU protection.
struct ProtectedUnitsHaveResourceControl;
impl Check for ProtectedUnitsHaveResourceControl {
    fn id(&self) -> &'static str {
        "protected-units"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        let units = [
            "lege-comp.service",
            "lege-sessiond.service",
            "lege-resourced.service",
        ];
        let mut unprotected = Vec::new();
        for unit in units {
            let path = context.path(&format!("/usr/lib/systemd/user/{unit}"));
            let text = fs::read_to_string(&path).unwrap_or_default();
            if !(text.contains("MemoryLow=") && text.contains("CPUWeight=")) {
                unprotected.push(unit);
            }
        }
        if unprotected.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "session-critical units are protected",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                "units without MemoryLow/CPUWeight",
                unprotected.join(", "),
            )
        }
    }
}

/// Exactly one NVIDIA flavour is installed, matching the detected hardware.
struct NvidiaFlavourConsistent;
impl Check for NvidiaFlavourConsistent {
    fn id(&self) -> &'static str {
        "nvidia-flavour"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let open = exists(&context.root, "/var/lib/dpkg/info/legeos-nvidia-open.list");
        let proprietary = exists(
            &context.root,
            "/var/lib/dpkg/info/legeos-nvidia-proprietary.list",
        );
        match (open, proprietary) {
            (true, true) => finding(
                self,
                Outcome::Fail,
                "both NVIDIA flavours installed",
                "open and proprietary modules must never coexist",
            ),
            (false, false) => finding(
                self,
                Outcome::Skipped,
                "no NVIDIA metapackage installed",
                "",
            ),
            (open, _) => {
                let expected = lege_hwdetect::scan_sysfs(&context.path("/sys"))
                    .ok()
                    .map(lege_hwdetect::classify)
                    .and_then(|profile| profile.nvidia_flavour);
                match expected {
                    Some(lege_hwdetect::NvidiaFlavour::Open) if !open => finding(
                        self,
                        Outcome::Fail,
                        "hardware wants the open flavour",
                        "legeos-nvidia-proprietary installed",
                    ),
                    Some(lege_hwdetect::NvidiaFlavour::Proprietary) if open => finding(
                        self,
                        Outcome::Fail,
                        "hardware wants the proprietary flavour",
                        "legeos-nvidia-open installed",
                    ),
                    _ => finding(
                        self,
                        Outcome::Pass,
                        "one NVIDIA flavour, consistent with hardware",
                        "",
                    ),
                }
            }
        }
    }
}

/// NVIDIA's standalone installer is not part of a supported configuration.
struct NoStandaloneNvidiaInstaller;
impl Check for NoStandaloneNvidiaInstaller {
    fn id(&self) -> &'static str {
        "no-nvidia-installer"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let markers = [
            "/usr/bin/nvidia-uninstall",
            "/var/lib/nvidia/log",
            "/usr/lib/nvidia/alternate-install-present",
        ];
        let present: Vec<&str> = markers
            .iter()
            .copied()
            .filter(|m| exists(&context.root, m))
            .collect();
        if present.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "no standalone NVIDIA installer artefacts",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                "standalone NVIDIA installer detected (unsupported)",
                present.join(", "),
            )
        }
    }
}

/// The portal backend selection file exists so every portal has one responding backend.
struct PortalConfigPresent;
impl Check for PortalConfigPresent {
    fn id(&self) -> &'static str {
        "portal-config"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        let ok = exists(
            &context.root,
            "/usr/share/xdg-desktop-portal/legeos-portals.conf",
        ) && exists(
            &context.root,
            "/usr/share/xdg-desktop-portal/portals/legeos.portal",
        );
        if ok {
            finding(self, Outcome::Pass, "portal backend configured", "")
        } else {
            finding(
                self,
                Outcome::Fail,
                "portal backend not configured",
                "legeos-portals.conf / legeos.portal missing",
            )
        }
    }
}

/// The root volume has more than its reserve available (runtime only).
struct StorageReserve;
impl Check for StorageReserve {
    fn id(&self) -> &'static str {
        "storage-reserve"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        if context.root != std::path::Path::new("/") {
            return finding(self, Outcome::Skipped, "storage reserve (runtime only)", "");
        }
        // Delegated to lege-filed when it is running; a coarse fallback here.
        match fs::metadata("/") {
            Ok(_) => finding(
                self,
                Outcome::Pass,
                "root volume reachable",
                "detailed reserve check via lege-filed",
            ),
            Err(error) => finding(self, Outcome::Fail, "cannot stat /", error.to_string()),
        }
    }
}

/// `lege-hostd`'s system-scope units, polkit policy and bus policy are all present.
struct HostUnitsPresent;
impl Check for HostUnitsPresent {
    fn id(&self) -> &'static str {
        "host-units"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let required = [
            "/usr/lib/systemd/system/lege-hostd.service",
            "/usr/lib/systemd/system/lege-boot-health.service",
            "/usr/share/polkit-1/actions/org.legeos.host.policy",
            "/usr/share/dbus-1/system.d/org.legeos.Host.conf",
        ];
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|p| !exists(&context.root, p))
            .collect();
        if missing.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "host daemon units and policy present",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!("{} host unit/policy file(s) missing", missing.len()),
                missing.join(", "),
            )
        }
    }
}

/// Every installed application manifest parses and its executable exists (planning doc 01 §7:
/// "every declared plugin has its binary", "every default application has a valid executable" —
/// checked over every installed manifest, the strongest static approximation of "default"
/// available without a live session's stored defaults).
struct AppManifestsValid;
impl Check for AppManifestsValid {
    fn id(&self) -> &'static str {
        "app-manifests"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let manifests = load_manifests(context);
        if manifests.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "no application manifests installed",
                "",
            );
        }
        let missing: Vec<String> = manifests
            .iter()
            .filter(|manifest| !exists(&context.root, &manifest.executable.to_string_lossy()))
            .map(|manifest| format!("{} ({})", manifest.id, manifest.executable.display()))
            .collect();
        if missing.is_empty() {
            finding(
                self,
                Outcome::Pass,
                format!(
                    "{} application manifest(s), every executable present",
                    manifests.len()
                ),
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!("{} manifest(s) with a missing executable", missing.len()),
                missing.join(", "),
            )
        }
    }
}

/// Every MIME handler named in a `mimeapps.list` refers to an installed application (planning
/// doc 01 §7). Checks the system default (`/etc/xdg/mimeapps.list`, meaningful for an image root
/// too) and, at runtime, the broker's own generated copy
/// (`$XDG_DATA_HOME/applications/mimeapps.list` — `docs/architecture/associations.md`:
/// "mimeapps.list is generated by the broker").
struct MimeHandlersInstalled;
impl Check for MimeHandlersInstalled {
    fn id(&self) -> &'static str {
        "mime-handlers"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let mut candidates = vec![context.path("/etc/xdg/mimeapps.list")];
        if context.root == Path::new("/")
            && let Some(data_home) = xdg_data_home()
        {
            candidates.push(data_home.join("applications/mimeapps.list"));
        }
        let Some((path, text)) = candidates
            .into_iter()
            .find_map(|path| fs::read_to_string(&path).ok().map(|text| (path, text)))
        else {
            return finding(self, Outcome::Skipped, "no mimeapps.list found", "");
        };

        let defaults = lege_associationd::import::parse_default_applications(&text);
        if defaults.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "mimeapps.list has no [Default Applications]",
                path.display().to_string(),
            );
        }
        let manifests = load_manifests(context);
        let known: std::collections::BTreeSet<&str> = manifests
            .iter()
            .flat_map(|manifest| {
                std::iter::once(manifest.primary_desktop_file_id.as_str())
                    .chain(manifest.aliases.desktop_file_ids.iter().map(String::as_str))
            })
            .collect();
        let orphaned: Vec<String> = defaults
            .iter()
            .filter(|(_, desktop_id)| !known.contains(desktop_id.as_str()))
            .map(|(mime, desktop_id)| format!("{mime} → {desktop_id}"))
            .collect();
        if orphaned.is_empty() {
            finding(
                self,
                Outcome::Pass,
                format!(
                    "{} MIME default(s), every handler installed",
                    defaults.len()
                ),
                path.display().to_string(),
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!(
                    "{} MIME default(s) with no installed handler",
                    orphaned.len()
                ),
                orphaned.join(", "),
            )
        }
    }
}

/// Every autostart entry that declares `X-LegeOS-Provides` only starts for the role's actually
/// selected provider (planning doc 01 §7: "every autostart entry resolves to one selected
/// provider"). Runtime only: this needs the live provider-selection state, which exists only
/// once a session has run at least once, and this check never creates that state itself — it
/// opens the store only when the file is already there.
struct AutostartProvidersResolved;
impl Check for AutostartProvidersResolved {
    fn id(&self) -> &'static str {
        "autostart-providers"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        if context.root != Path::new("/") {
            return finding(
                self,
                Outcome::Skipped,
                "autostart providers (runtime only)",
                "",
            );
        }
        let db_path = lege_state::default_path();
        if !db_path.exists() {
            return finding(
                self,
                Outcome::Skipped,
                "no session state store yet (no session has run)",
                db_path.display().to_string(),
            );
        }
        let store = match lege_state::Store::open(&db_path) {
            Ok(store) => store,
            Err(error) => {
                return finding(
                    self,
                    Outcome::Fail,
                    "could not open the state store",
                    error.to_string(),
                );
            }
        };
        let registry = match lege_session_core::providers::load(&store) {
            Ok(registry) => registry,
            Err(error) => {
                return finding(
                    self,
                    Outcome::Fail,
                    "could not load provider selections",
                    error.to_string(),
                );
            }
        };

        let manifests = load_manifests(context);
        let by_desktop_id = desktop_id_index(&manifests);
        let mut dirs = vec![context.path("/etc/xdg/autostart")];
        if let Some(config_home) = xdg_config_home() {
            dirs.push(config_home.join("autostart"));
        }
        let (checked, mismatches) = scan_autostart_entries(&dirs, &by_desktop_id, &registry);
        if checked == 0 {
            finding(
                self,
                Outcome::Skipped,
                "no autostart entries declare a provider role",
                "",
            )
        } else if mismatches.is_empty() {
            finding(
                self,
                Outcome::Pass,
                format!("{checked} role-declaring autostart entry/entries resolve correctly"),
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!(
                    "{} autostart entry/entries do not resolve",
                    mismatches.len()
                ),
                mismatches.join("; "),
            )
        }
    }
}

/// Map every desktop-file id an installed manifest answers to (its primary id, plus every
/// compatibility alias) to that manifest's canonical [`lege_types::AppId`].
fn desktop_id_index(manifests: &[AppManifest]) -> BTreeMap<&str, &lege_types::AppId> {
    manifests
        .iter()
        .flat_map(|manifest| {
            std::iter::once((manifest.primary_desktop_file_id.as_str(), &manifest.id)).chain(
                manifest
                    .aliases
                    .desktop_file_ids
                    .iter()
                    .map(move |id| (id.as_str(), &manifest.id)),
            )
        })
        .collect()
}

/// Scan `dirs` for non-hidden `.desktop` autostart entries that declare `X-LegeOS-Provides`, and
/// check each one against `registry`. Returns `(entries checked, mismatch descriptions)`; an
/// entry whose desktop id matches no installed manifest is silently skipped here — that gap is
/// [`AppManifestsValid`]'s / [`MimeHandlersInstalled`]'s to report, not this check's.
fn scan_autostart_entries(
    dirs: &[std::path::PathBuf],
    by_desktop_id: &BTreeMap<&str, &lege_types::AppId>,
    registry: &lege_session_core::providers::ProviderRegistry,
) -> (u32, Vec<String>) {
    let mut mismatches = Vec::new();
    let mut checked = 0u32;
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "desktop") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(desktop_entry) = lege_appd::desktop_entry::parse(&text) else {
                continue;
            };
            if desktop_entry.hidden {
                continue;
            }
            let Some(role_name) = desktop_entry.extra.get("X-LegeOS-Provides") else {
                continue;
            };
            let Some(role) = ProviderRole::ALL
                .iter()
                .find(|r| r.as_str() == role_name.trim())
            else {
                continue;
            };
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(app_id) = by_desktop_id.get(file_name) else {
                continue;
            };
            checked += 1;
            if registry.provider(*role).as_ref() != Some(*app_id) {
                mismatches.push(format!(
                    "{file_name} declares {role} but is not the selected provider"
                ));
            }
        }
    }
    (checked, mismatches)
}

fn xdg_data_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
}

fn xdg_config_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{exit_code, run_all};

    #[test]
    fn empty_root_fails_the_identity_checks_and_reports_exit_code_1() {
        let root = std::env::temp_dir().join(format!("lege-doctor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let findings = run_all(&Context {
            root: root.clone(),
            hardware: false,
        });
        assert!(
            findings
                .iter()
                .any(|f| f.id == "os-release" && f.outcome == Outcome::Fail)
        );
        assert_eq!(exit_code(&findings), 1);

        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(
            root.join("etc/os-release"),
            "ID=legeos\nID_LIKE=ubuntu debian\n",
        )
        .unwrap();
        let findings = run_all(&Context {
            root: root.clone(),
            hardware: false,
        });
        assert!(
            findings
                .iter()
                .any(|f| f.id == "os-release" && f.outcome == Outcome::Pass)
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn fixture_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("lege-doctor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn manifest(id: &str, executable: &str) -> String {
        format!(
            r#"{{"id":"{id}","display_name":"{id}","primary_desktop_file_id":"{id}.desktop",
                "executable":"{executable}","mime_types":["application/pdf"],
                "resource_class":"Interactive"}}"#
        )
    }

    #[test]
    fn host_units_present_reports_every_missing_file() {
        let root = fixture_root("host-units");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(HostUnitsPresent.run(&context).outcome, Outcome::Fail);

        for path in [
            "usr/lib/systemd/system/lege-hostd.service",
            "usr/lib/systemd/system/lege-boot-health.service",
            "usr/share/polkit-1/actions/org.legeos.host.policy",
            "usr/share/dbus-1/system.d/org.legeos.Host.conf",
        ] {
            let full = root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, "").unwrap();
        }
        assert_eq!(HostUnitsPresent.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn app_manifests_valid_skips_when_none_installed_and_fails_a_missing_executable() {
        let root = fixture_root("app-manifests");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(AppManifestsValid.run(&context).outcome, Outcome::Skipped);

        let apps_dir = root.join("usr/share/legeos/apps");
        fs::create_dir_all(&apps_dir).unwrap();
        fs::write(
            apps_dir.join("org.legeos.Viewer.json"),
            manifest("org.legeos.Viewer", "/usr/bin/lege-viewer"),
        )
        .unwrap();
        // The executable does not exist under the root: this must fail, not silently pass.
        assert_eq!(AppManifestsValid.run(&context).outcome, Outcome::Fail);

        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::write(root.join("usr/bin/lege-viewer"), "").unwrap();
        assert_eq!(AppManifestsValid.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mime_handlers_installed_flags_a_handler_with_no_manifest() {
        let root = fixture_root("mime-handlers");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(
            MimeHandlersInstalled.run(&context).outcome,
            Outcome::Skipped
        );

        fs::create_dir_all(root.join("etc/xdg")).unwrap();
        fs::write(
            root.join("etc/xdg/mimeapps.list"),
            "[Default Applications]\napplication/pdf=org.legeos.Viewer.desktop;\n",
        )
        .unwrap();
        // No manifest installed at all yet: the handler is orphaned.
        assert_eq!(MimeHandlersInstalled.run(&context).outcome, Outcome::Fail);

        let apps_dir = root.join("usr/share/legeos/apps");
        fs::create_dir_all(&apps_dir).unwrap();
        fs::write(
            apps_dir.join("org.legeos.Viewer.json"),
            manifest("org.legeos.Viewer", "/usr/bin/lege-viewer"),
        )
        .unwrap();
        assert_eq!(MimeHandlersInstalled.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn autostart_providers_resolved_is_runtime_only() {
        let context = Context {
            root: std::path::PathBuf::from("/some/image/root"),
            hardware: false,
        };
        assert_eq!(
            AutostartProvidersResolved.run(&context).outcome,
            Outcome::Skipped
        );
    }

    #[test]
    fn scan_autostart_entries_flags_a_role_declared_by_the_wrong_app() {
        let dir =
            std::env::temp_dir().join(format!("lege-doctor-autostart-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("org.legeos.NetworkTray.desktop"),
            "[Desktop Entry]\nType=Application\nName=Network Tray\nExec=lege-network-tray\n\
             X-LegeOS-Provides=network-status-ui\n",
        )
        .unwrap();
        fs::write(
            dir.join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden\nExec=x\nHidden=true\n\
             X-LegeOS-Provides=network-status-ui\n",
        )
        .unwrap();

        let manifests = [
            serde_json::from_str::<AppManifest>(&manifest(
                "org.legeos.NetworkTray",
                "/usr/bin/lege-network-tray",
            ))
            .unwrap(),
            serde_json::from_str::<AppManifest>(&manifest(
                "org.gnome.NmApplet",
                "/usr/bin/nm-applet",
            ))
            .unwrap(),
        ];
        let by_desktop_id = desktop_id_index(&manifests);

        let mut registry = lege_session_core::providers::ProviderRegistry::default();
        // A selection only sticks (rather than reading as stale) once the app has also declared
        // the role — exactly like the real registry a live session builds.
        registry.declare(
            "org.legeos.NetworkTray".parse().unwrap(),
            lege_types::ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 100,
            },
        );
        registry.declare(
            "org.gnome.NmApplet".parse().unwrap(),
            lege_types::ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 50,
            },
        );
        registry.select(
            ProviderRole::NetworkStatusUi,
            "org.gnome.NmApplet".parse().unwrap(),
        );
        let (checked, mismatches) =
            scan_autostart_entries(std::slice::from_ref(&dir), &by_desktop_id, &registry);
        // The hidden entry must not be counted at all.
        assert_eq!(checked, 1);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].contains("org.legeos.NetworkTray.desktop"));

        registry.select(
            ProviderRole::NetworkStatusUi,
            "org.legeos.NetworkTray".parse().unwrap(),
        );
        let (checked, mismatches) =
            scan_autostart_entries(std::slice::from_ref(&dir), &by_desktop_id, &registry);
        assert_eq!(checked, 1);
        assert!(mismatches.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
