//! Deriving [`AppManifest`]s from installed `.desktop` files for applications that ship no
//! native LegeOS manifest (`docs/architecture/identity.md` marks this a follow-up; this is it).
//!
//! A real manifest always wins: the caller (`lege-sessiond`'s startup sequence) loads native
//! manifests first and skips importing any `.desktop` file whose ID is already claimed by one
//! (see `primary_desktop_file_id` / `aliases.desktop_file_ids`), so a derived manifest can only
//! ever fill a gap, never shadow a real one.
//!
//! `Exec=` is parsed into an [`ExecTemplate`] — a sequence of literal words and field codes
//! (`%f %F %u %U %i %c %k`, plus the deprecated `%d %D %n %N %v %m`, which are dropped) — and
//! expanded into an argument vector at launch time, never handed to a shell.

use lege_types::{AppAliases, AppId, AppManifest, ResourceClass};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
};

/// One `[Desktop Entry]` group, the fields LegeOS cares about. Localized keys (`Name[xx]`) are
/// skipped; only the unlocalized value is read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub entry_type: String,
    pub name: String,
    pub exec: Option<String>,
    pub try_exec: Option<String>,
    pub icon: Option<String>,
    pub mime_types: Vec<String>,
    pub startup_wm_class: Option<String>,
    pub no_display: bool,
    pub hidden: bool,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
    pub x_flatpak: Option<String>,
    /// Every other `X-`-prefixed key, verbatim. `lege-sessiond`'s autostart importer reads
    /// `X-LegeOS-Background` (comma-separated [`lege_session_core::autostart::BackgroundPermission`]
    /// kebab-names) and `X-LegeOS-Provides` (a single [`lege_types::ProviderRole`] kebab-name)
    /// from here; nothing in this crate interprets them.
    pub extra: BTreeMap<String, String>,
}

/// Parse the `[Desktop Entry]` group of a `.desktop` file. `None` if the group is absent.
pub fn parse(text: &str) -> Option<DesktopEntry> {
    let mut entry = DesktopEntry::default();
    let mut in_main_group = false;
    let mut saw_main_group = false;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_main_group = name == "Desktop Entry";
            saw_main_group |= in_main_group;
            continue;
        }
        if !in_main_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.contains('[') {
            continue; // localized variant; the unlocalized key wins.
        }
        match key {
            "Type" => value.clone_into(&mut entry.entry_type),
            "Name" => entry.name = unescape(value),
            "Exec" => entry.exec = Some(value.to_owned()),
            "TryExec" => entry.try_exec = Some(value.to_owned()),
            "Icon" => entry.icon = Some(unescape(value)),
            "MimeType" => entry.mime_types = split_list(value),
            "StartupWMClass" => entry.startup_wm_class = Some(value.to_owned()),
            "NoDisplay" => entry.no_display = is_true(value),
            "Hidden" => entry.hidden = is_true(value),
            "OnlyShowIn" => entry.only_show_in = split_list(value),
            "NotShowIn" => entry.not_show_in = split_list(value),
            "X-Flatpak" => entry.x_flatpak = Some(value.to_owned()),
            _ if key.starts_with("X-") => {
                entry.extra.insert(key.to_owned(), value.to_owned());
            }
            _ => {}
        }
    }

    saw_main_group.then_some(entry)
}

fn is_true(value: &str) -> bool {
    value.eq_ignore_ascii_case("true")
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The handful of backslash escapes the spec defines for localestring/string values.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// One word of a parsed `Exec=` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecToken {
    Literal(String),
    SingleFile,
    MultipleFiles,
    SingleUrl,
    MultipleUrls,
    Icon,
    TranslatedName,
    DesktopFilePath,
}

/// `Exec=` parsed into words, field codes intact — never expanded through a shell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecTemplate(Vec<ExecToken>);

impl ExecTemplate {
    pub fn parse(line: &str) -> Self {
        Self(
            split_words(line)
                .iter()
                .flat_map(|word| parse_word(word))
                .collect(),
        )
    }

    /// The literal program path/name in argv\[0\], if the template starts with one (a wrapper
    /// such as `env FOO=bar prog %f` — whose first word is not a plain literal `env` either,
    /// since it *is* one — parses fine; only a template that *starts* with a field code, which
    /// the spec does not allow, yields `None`).
    pub fn program_literal(&self) -> Option<&str> {
        match self.0.first() {
            Some(ExecToken::Literal(word)) => Some(word.as_str()),
            _ => None,
        }
    }

    /// Expand into the arguments *after* argv\[0\] (the launcher already has the resolved
    /// executable as argv\[0\]). `targets` are the files/URIs to substitute for `%f/%F/%u/%U`.
    pub fn expand_arguments(
        &self,
        targets: &[String],
        icon: &str,
        translated_name: &str,
        desktop_file_path: &str,
    ) -> Vec<String> {
        let mut arguments = Vec::new();
        for token in self.0.iter().skip(1) {
            match token {
                ExecToken::Literal(word) => arguments.push(word.clone()),
                ExecToken::SingleFile | ExecToken::SingleUrl => {
                    if let Some(target) = targets.first() {
                        arguments.push(target.clone());
                    }
                }
                ExecToken::MultipleFiles | ExecToken::MultipleUrls => {
                    arguments.extend(targets.iter().cloned());
                }
                ExecToken::Icon => {
                    if !icon.is_empty() {
                        arguments.push("--icon".into());
                        arguments.push(icon.to_owned());
                    }
                }
                ExecToken::TranslatedName => arguments.push(translated_name.to_owned()),
                ExecToken::DesktopFilePath => arguments.push(desktop_file_path.to_owned()),
            }
        }
        arguments
    }
}

/// Split on whitespace, honouring double-quoted segments and the spec's limited backslash
/// escapes inside them (`\"`, `` \` ``, `\$`, `\\`).
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut touched = false;
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                touched = true;
            }
            '\\' if in_quotes => match chars.peek().copied() {
                Some(next @ ('"' | '\\' | '`' | '$')) => {
                    current.push(next);
                    chars.next();
                }
                _ => current.push('\\'),
            },
            c if c.is_whitespace() && !in_quotes => {
                if touched {
                    words.push(std::mem::take(&mut current));
                    touched = false;
                }
            }
            c => {
                current.push(c);
                touched = true;
            }
        }
    }
    if touched {
        words.push(current);
    }
    words
}

fn parse_word(word: &str) -> Vec<ExecToken> {
    match word {
        "%f" => return vec![ExecToken::SingleFile],
        "%F" => return vec![ExecToken::MultipleFiles],
        "%u" => return vec![ExecToken::SingleUrl],
        "%U" => return vec![ExecToken::MultipleUrls],
        "%i" => return vec![ExecToken::Icon],
        "%c" => return vec![ExecToken::TranslatedName],
        "%k" => return vec![ExecToken::DesktopFilePath],
        "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => return Vec::new(), // deprecated; dropped.
        _ => {}
    }

    // A mixed word: only %% (literal percent), %c and %k are meaningful embedded; any other
    // embedded field code is dropped (its own standalone form is handled above, and embedding
    // %f/%F/%u/%U is not valid per the spec).
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            literal.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => literal.push('%'),
            Some('c') => {
                if !literal.is_empty() {
                    tokens.push(ExecToken::Literal(std::mem::take(&mut literal)));
                }
                tokens.push(ExecToken::TranslatedName);
            }
            Some('k') => {
                if !literal.is_empty() {
                    tokens.push(ExecToken::Literal(std::mem::take(&mut literal)));
                }
                tokens.push(ExecToken::DesktopFilePath);
            }
            Some(_) | None => {} // unsupported embedded code: drop.
        }
    }
    if !literal.is_empty() || tokens.is_empty() {
        tokens.push(ExecToken::Literal(literal));
    }
    tokens
}

/// Resolves an `Exec=`/`TryExec=` program name (or absolute path) and a Flatpak application ID
/// to an executable file. A seam so import can be tested without touching `$PATH` or a real
/// Flatpak installation.
pub trait ExecResolver: Send + Sync {
    fn resolve_program(&self, name_or_path: &str) -> Option<PathBuf>;
    fn resolve_flatpak(&self, flatpak_id: &str) -> Option<PathBuf>;
}

/// The real resolver: searches `$PATH`, and Flatpak's per-app export wrapper directories (which
/// are already directly executable — never `flatpak run` through a shell).
#[derive(Clone, Debug)]
pub struct PathExecResolver {
    path_dirs: Vec<PathBuf>,
    flatpak_export_dirs: Vec<PathBuf>,
}

impl PathExecResolver {
    pub fn from_env() -> Self {
        let path_dirs = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        let mut flatpak_export_dirs = vec![PathBuf::from("/var/lib/flatpak/exports/bin")];
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
        {
            flatpak_export_dirs.push(data_home.join("flatpak/exports/bin"));
        }
        Self {
            path_dirs,
            flatpak_export_dirs,
        }
    }
}

impl ExecResolver for PathExecResolver {
    fn resolve_program(&self, name_or_path: &str) -> Option<PathBuf> {
        let path = Path::new(name_or_path);
        if path.is_absolute() {
            return is_executable_file(path).then(|| path.to_path_buf());
        }
        self.path_dirs
            .iter()
            .map(|dir| dir.join(name_or_path))
            .find(|candidate| is_executable_file(candidate))
    }

    fn resolve_flatpak(&self, flatpak_id: &str) -> Option<PathBuf> {
        self.flatpak_export_dirs
            .iter()
            .map(|dir| dir.join(flatpak_id))
            .find(|candidate| is_executable_file(candidate))
    }
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Derive the canonical [`AppId`] LegeOS uses for a `.desktop` file it did not ship a manifest
/// for. Namespaced under `desktop-import.` so it can never collide with (or shadow) a native
/// manifest's ID, and so `lege doctor`/Settings can plainly label it as imported.
pub fn derive_app_id(desktop_id: &str) -> AppId {
    let sanitized: String = desktop_id
        .chars()
        .take(230)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    };
    AppId::from_str(&format!("desktop-import.{sanitized}"))
        .unwrap_or_else(|_| AppId::from_str("desktop-import.unknown").expect("static id is valid"))
}

/// The result of deriving a manifest from a `.desktop` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedManifest {
    pub manifest: AppManifest,
    pub exec_template: ExecTemplate,
}

/// Derive a manifest from `entry` (the `.desktop` file whose merged ID is `desktop_id`, e.g.
/// `org.mozilla.firefox.desktop`), or `None` if it is not an importable application.
///
/// Skips: non-`Application` types, `Hidden=true` (the standard "this entry is masked" signal —
/// unlike `NoDisplay`, which only hides it from menus and does not affect import), entries with
/// no `Exec=`, and entries whose executable does not resolve (including a `TryExec=` gate that
/// fails, exactly as the spec intends it: "don't offer this even though the file exists").
pub fn derive_manifest(
    entry: &DesktopEntry,
    desktop_id: &str,
    resolver: &dyn ExecResolver,
) -> Option<DerivedManifest> {
    if entry.entry_type != "Application" || entry.hidden {
        return None;
    }
    let exec = entry.exec.as_deref()?;
    let template = ExecTemplate::parse(exec);
    let program = template.program_literal()?;

    if let Some(try_exec) = entry.try_exec.as_deref()
        && resolver.resolve_program(try_exec).is_none()
    {
        return None;
    }

    let executable = match entry.x_flatpak.as_deref() {
        Some(flatpak_id) => resolver.resolve_flatpak(flatpak_id)?,
        None => resolver.resolve_program(program)?,
    };

    let app_id = derive_app_id(desktop_id);
    let mut aliases = AppAliases {
        desktop_file_ids: [desktop_id.to_owned()].into(),
        ..AppAliases::default()
    };
    if let Some(class) = &entry.startup_wm_class {
        aliases.startup_wm_classes.insert(class.clone());
    } else if let Some(base) = desktop_id.strip_suffix(".desktop") {
        // Common toolkit convention when StartupWMClass is absent: the client's app_id/WM_CLASS
        // defaults to the desktop file's base name. Evidence only — never trusted over a cgroup.
        aliases.startup_wm_classes.insert(base.to_owned());
        aliases.wayland_app_ids.insert(base.to_owned());
    }
    if let Some(flatpak_id) = &entry.x_flatpak {
        aliases.flatpak_ids.insert(flatpak_id.clone());
    }
    aliases.executable_paths.insert(executable.clone());

    let display_name = if entry.name.is_empty() {
        desktop_id.trim_end_matches(".desktop").to_owned()
    } else {
        entry.name.clone()
    };

    let manifest = AppManifest {
        id: app_id,
        display_name,
        primary_desktop_file_id: desktop_id.to_owned(),
        executable,
        aliases,
        mime_types: entry.mime_types.iter().cloned().collect(),
        resource_class: ResourceClass::Normal,
        supports_checkpoint: false,
    };
    Some(DerivedManifest {
        manifest,
        exec_template: template,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct StaticResolver {
        programs: HashMap<&'static str, &'static str>,
        flatpaks: HashMap<&'static str, &'static str>,
    }

    impl ExecResolver for StaticResolver {
        fn resolve_program(&self, name_or_path: &str) -> Option<PathBuf> {
            self.programs.get(name_or_path).map(PathBuf::from)
        }
        fn resolve_flatpak(&self, flatpak_id: &str) -> Option<PathBuf> {
            self.flatpaks.get(flatpak_id).map(PathBuf::from)
        }
    }

    const VIEWER: &str = "\
[Desktop Entry]
Type=Application
Name=Lege Viewer
Exec=lege-viewer --reader %f
TryExec=lege-viewer
MimeType=application/pdf;image/tiff;
StartupWMClass=lege-viewer
Icon=lege-viewer
";

    #[test]
    fn parses_the_main_group_and_ignores_actions() {
        let text = format!(
            "{VIEWER}\n[Desktop Action new-window]\nExec=lege-viewer --new\nName=New Window\n"
        );
        let entry = parse(&text).unwrap();
        assert_eq!(entry.name, "Lege Viewer");
        assert_eq!(entry.exec.as_deref(), Some("lege-viewer --reader %f"));
        assert_eq!(entry.mime_types, vec!["application/pdf", "image/tiff"]);
        assert_eq!(entry.startup_wm_class.as_deref(), Some("lege-viewer"));
        assert!(!entry.hidden);
        assert!(!entry.no_display);
    }

    #[test]
    fn localized_names_do_not_override_the_default() {
        let text =
            "[Desktop Entry]\nType=Application\nName=Viewer\nName[fr]=Visionneuse\nExec=viewer\n";
        let entry = parse(text).unwrap();
        assert_eq!(entry.name, "Viewer");
    }

    #[test]
    fn exec_template_expands_field_codes() {
        let template = ExecTemplate::parse("lege-viewer --icon %i --title %c %F");
        let args = template.expand_arguments(
            &["/tmp/a.pdf".into(), "/tmp/b.pdf".into()],
            "lege-viewer",
            "Lege Viewer",
            "/usr/share/applications/org.legeos.Viewer.desktop",
        );
        assert_eq!(
            args,
            vec![
                "--icon",
                "--icon",
                "lege-viewer",
                "--title",
                "Lege Viewer",
                "/tmp/a.pdf",
                "/tmp/b.pdf",
            ]
        );
    }

    #[test]
    fn single_file_code_takes_only_the_first_target() {
        let template = ExecTemplate::parse("lege-viewer %f");
        let args = template.expand_arguments(&["a.pdf".into(), "b.pdf".into()], "", "", "");
        assert_eq!(args, vec!["a.pdf"]);
        assert_eq!(
            ExecTemplate::parse("lege-viewer %f").expand_arguments(&[], "", "", ""),
            Vec::<String>::new()
        );
    }

    #[test]
    fn percent_percent_and_deprecated_codes() {
        let template = ExecTemplate::parse("prog 100%% %D --flag");
        assert_eq!(
            template.expand_arguments(&[], "", "", ""),
            vec!["100%", "--flag"]
        );
    }

    #[test]
    fn quoted_words_keep_embedded_spaces() {
        let words = split_words(r#"prog "--title=My App" %f"#);
        assert_eq!(words, vec!["prog", "--title=My App", "%f"]);
    }

    #[test]
    fn derive_app_id_is_namespaced_and_deterministic() {
        let id = derive_app_id("firefox.desktop");
        assert_eq!(id.as_str(), "desktop-import.firefox.desktop");
        assert_eq!(id, derive_app_id("firefox.desktop"));
        assert_ne!(id, derive_app_id("firefox-esr.desktop"));
    }

    #[test]
    fn hidden_entries_are_skipped_but_no_display_entries_import() {
        let resolver = StaticResolver {
            programs: HashMap::from([("lege-viewer", "/usr/bin/lege-viewer")]),
            flatpaks: HashMap::new(),
        };
        let mut hidden = parse(VIEWER).unwrap();
        hidden.hidden = true;
        assert!(derive_manifest(&hidden, "org.legeos.Viewer.desktop", &resolver).is_none());

        let mut no_display = parse(VIEWER).unwrap();
        no_display.no_display = true;
        assert!(derive_manifest(&no_display, "org.legeos.Viewer.desktop", &resolver).is_some());
    }

    #[test]
    fn try_exec_gates_import_when_it_fails_to_resolve() {
        let resolver = StaticResolver {
            programs: HashMap::new(),
            flatpaks: HashMap::new(),
        };
        let entry = parse(VIEWER).unwrap();
        assert!(derive_manifest(&entry, "org.legeos.Viewer.desktop", &resolver).is_none());
    }

    #[test]
    fn derives_a_usable_manifest_and_exec_template() {
        let resolver = StaticResolver {
            programs: HashMap::from([("lege-viewer", "/usr/bin/lege-viewer")]),
            flatpaks: HashMap::new(),
        };
        let entry = parse(VIEWER).unwrap();
        let derived = derive_manifest(&entry, "org.legeos.Viewer.desktop", &resolver).unwrap();
        assert_eq!(
            derived.manifest.executable,
            PathBuf::from("/usr/bin/lege-viewer")
        );
        assert_eq!(derived.manifest.display_name, "Lege Viewer");
        assert!(derived.manifest.supports_mime("application/pdf"));
        assert!(
            derived
                .manifest
                .aliases
                .desktop_file_ids
                .contains("org.legeos.Viewer.desktop")
        );
        assert_eq!(
            derived
                .exec_template
                .expand_arguments(&["a.pdf".into()], "", "", ""),
            vec!["--reader", "a.pdf"]
        );
    }

    #[test]
    fn flatpak_entries_resolve_through_the_export_wrapper() {
        let resolver = StaticResolver {
            programs: HashMap::new(),
            flatpaks: HashMap::from([(
                "org.mozilla.firefox",
                "/var/lib/flatpak/exports/bin/org.mozilla.firefox",
            )]),
        };
        let text = "[Desktop Entry]\nType=Application\nName=Firefox\nExec=/usr/bin/flatpak run org.mozilla.firefox %u\nX-Flatpak=org.mozilla.firefox\n";
        let entry = parse(text).unwrap();
        let derived = derive_manifest(&entry, "org.mozilla.firefox.desktop", &resolver).unwrap();
        assert_eq!(
            derived.manifest.executable,
            PathBuf::from("/var/lib/flatpak/exports/bin/org.mozilla.firefox")
        );
        assert!(
            derived
                .manifest
                .aliases
                .flatpak_ids
                .contains("org.mozilla.firefox")
        );
    }

    #[test]
    fn missing_start_wm_class_falls_back_to_the_desktop_id_base_name() {
        let resolver = StaticResolver {
            programs: HashMap::from([("prog", "/usr/bin/prog")]),
            flatpaks: HashMap::new(),
        };
        let text = "[Desktop Entry]\nType=Application\nName=Prog\nExec=prog\n";
        let entry = parse(text).unwrap();
        let derived = derive_manifest(&entry, "prog.desktop", &resolver).unwrap();
        assert!(derived.manifest.aliases.startup_wm_classes.contains("prog"));
        assert!(derived.manifest.aliases.wayland_app_ids.contains("prog"));
    }

    #[test]
    fn x_keys_are_captured_for_the_autostart_importer() {
        let text = "[Desktop Entry]\nType=Application\nName=Sync\nExec=sync-agent\nX-LegeOS-Background=continuous-background,network-while-hidden\nX-LegeOS-Provides=notification-server\n";
        let entry = parse(text).unwrap();
        assert_eq!(
            entry.extra.get("X-LegeOS-Background").map(String::as_str),
            Some("continuous-background,network-while-hidden")
        );
        assert_eq!(
            entry.extra.get("X-LegeOS-Provides").map(String::as_str),
            Some("notification-server")
        );
    }
}
