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

use lege_types::{
    AppAliases, AppId, AppManifest, AppOriginKind, AppProvenance, AppVariantId, LaunchMechanism,
    ResourceClass,
};
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
    /// `Categories=`, verbatim. Read by launchers; LegeOS itself attaches no meaning to them.
    pub categories: Vec<String>,
    /// `Keywords=`: the search terms the application suggests for itself.
    pub keywords: Vec<String>,
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
            "Categories" => entry.categories = split_escaped_list(value),
            // Keywords is a localestring list; the unlocalized form is the only one read, the
            // same rule the `key.contains('[')` skip above applies to every other key.
            "Keywords" => entry.keywords = split_escaped_list(value),
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

/// Split a `;`-separated list whose entries may carry the spec's backslash escapes, then unescape
/// each one. Unlike [`split_list`] this splits *before* unescaping, so an entry containing a
/// literal `\;` stays one entry rather than silently becoming two — which for `Keywords=` is the
/// difference between one search term and two nonsense ones.
fn split_escaped_list(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' if chars.peek() == Some(&';') => {
                chars.next();
                current.push(';');
            }
            ';' => parts.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| unescape(part.trim()))
        .filter(|part| !part.is_empty())
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

/// Which family of directories a `.desktop` file was found in.
///
/// This is the key an import scan groups candidates by, and it is the fix for a live bug: keyed on
/// the desktop id alone, a Flatpak export of `firefox.desktop` and the distribution's own
/// `firefox.desktop` collapse into one candidate, and whichever directory is scanned last silently
/// erases the other application from the registry. Within the XDG chain that shadowing is
/// *correct* — a person's own `~/.local/share/applications/firefox.desktop` is meant to replace the
/// system one — so the chain stays one namespace and Flatpak becomes another.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OriginNamespace {
    /// `/usr/share/applications`, `$XDG_DATA_HOME/applications`, and anything else in the XDG
    /// precedence chain. Later directories shadow earlier ones for the same desktop id.
    #[default]
    Xdg,
    /// A Flatpak export directory. Its entries never shadow, and are never shadowed by, the XDG
    /// chain.
    Flatpak,
}

/// Where a `.desktop` file was found, and therefore what may be concluded about it.
///
/// Passed to [`derive_manifest`] so provenance is recorded at the one moment it is actually known:
/// the scan that read the file. Re-deriving it later from the manifest would be a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceContext {
    /// The namespace an import scan keys this candidate under.
    pub namespace: OriginNamespace,
    /// What kind of place the containing directory is. Determines the trust tier.
    pub origin: AppOriginKind,
    /// The `.desktop` file itself.
    pub path: PathBuf,
}

impl SourceContext {
    /// A source in the XDG chain with an unknown origin — the conservative reading, used where a
    /// caller supplied a directory the system knows nothing about (a `--application-dir` override,
    /// a test fixture).
    pub fn unknown(path: PathBuf) -> Self {
        Self {
            namespace: OriginNamespace::Xdg,
            origin: AppOriginKind::Unknown,
            path,
        }
    }

    /// Classify one application directory. Path-shaped rather than clever: the three cases that
    /// exist on a real install are a Flatpak export tree, a system application directory, and the
    /// person's own. Anything else is unknown, and unknown is untrusted.
    pub fn classify_directory(directory: &Path) -> (OriginNamespace, AppOriginKind) {
        let text = directory.to_string_lossy();
        if text.contains("/flatpak/exports/share/applications") {
            (OriginNamespace::Flatpak, AppOriginKind::Flatpak)
        } else if text.starts_with("/usr/share/applications")
            || text.starts_with("/usr/local/share/applications")
            || text.starts_with("/var/lib/snapd/desktop/applications")
            || text.starts_with("/etc/xdg")
        {
            (OriginNamespace::Xdg, AppOriginKind::DistroPackage)
        } else if is_below_home_data(&text) {
            (OriginNamespace::Xdg, AppOriginKind::UserDesktopEntry)
        } else {
            (OriginNamespace::Xdg, AppOriginKind::Unknown)
        }
    }

    /// The source context for `path`, a `.desktop` file directly inside `directory`.
    pub fn for_file(directory: &Path, path: PathBuf) -> Self {
        let (namespace, origin) = Self::classify_directory(directory);
        Self {
            namespace,
            origin,
            path,
        }
    }
}

fn is_below_home_data(text: &str) -> bool {
    text.starts_with("/home/") || text.starts_with("/root/")
}

/// The variant id for one `.desktop` file: which *copy* of an application this is.
///
/// A Flatpak export is keyed on its application id (`flatpak.org.mozilla.firefox`) rather than on
/// the desktop file it happens to export, so the distribution package and the Flatpak of the same
/// program hold two different ids and can both exist. Every other entry keeps the id it has always
/// had — stored ids elsewhere are never rewritten, only resolved through an alias.
pub fn derive_variant_id(desktop_id: &str, flatpak_id: Option<&str>) -> AppVariantId {
    let Some(flatpak_id) = flatpak_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return derive_app_id(desktop_id);
    };
    let sanitized: String = flatpak_id
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
    AppId::from_str(&format!("flatpak.{sanitized}")).unwrap_or_else(|_| derive_app_id(desktop_id))
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
///
/// `NoDisplay` is *recorded* rather than acted on here ([`AppManifest::no_display`]): the entry
/// must still import, because a MIME association may resolve to it and the launcher must still
/// be able to start it — it is only the offered-in-a-list step that honours the flag.
///
/// `OnlyShowIn` / `NotShowIn` fold into that same flag ([`hidden_from_desktop`]): an entry meant
/// for another desktop imports like any other and is simply not offered here.
///
/// `source` says where the file was found. It is taken here rather than reconstructed later
/// because this call is the one moment the origin is actually known; anything derived afterwards
/// from the manifest alone would be a guess, and provenance that guesses is worse than none.
/// Whether `entry` asks not to be shown on a desktop named by `current`.
///
/// `OnlyShowIn` and `NotShowIn` were parsed and thrown away, which is how `pcmanfm-qt`'s file
/// manager became unreachable: `/etc/xdg/autostart/lxqt-desktop.desktop` is `OnlyShowIn=LXQt` and
/// runs `pcmanfm-qt --desktop`, and being the oldest variant of that executable it won the
/// default-variant election ([`crate::variants::default_rank`], which sorts on `no_display`
/// first). The launcher's only row for it was therefore named "Desktop" and painted a desktop
/// instead of opening a window. Honouring the two keys demotes it and the real entry wins.
///
/// The match is case-sensitive per the spec, and `NotShowIn` is checked first because an entry
/// naming both means the exclusion.
pub fn hidden_from_desktop(entry: &DesktopEntry, current: &[String]) -> bool {
    let names = |list: &[String]| {
        list.iter()
            .any(|name| current.iter().any(|here| here == name))
    };
    if !entry.not_show_in.is_empty() && names(&entry.not_show_in) {
        return true;
    }
    !entry.only_show_in.is_empty() && !names(&entry.only_show_in)
}

/// The desktop names this session answers to, from `XDG_CURRENT_DESKTOP` (colon-separated).
///
/// From the environment rather than a constant so appd behaves like whatever session it is
/// actually running in — a nested session under another desktop hides that desktop's entries, not
/// LegeOS's. `distro/policy/environment.d/60-legeos.conf` sets `XDG_CURRENT_DESKTOP=LegeOS`, which
/// is also the fallback: unset, this is a LegeOS session that lost its environment, and importing
/// every other desktop's autostart entries is the worse of the two failures.
fn current_desktops() -> Vec<String> {
    desktops_from(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref())
}

/// [`current_desktops`] without the environment, so the parsing and the fallback are testable in
/// a crate that forbids the `unsafe` block `set_var` now needs.
fn desktops_from(value: Option<&str>) -> Vec<String> {
    let names: Vec<String> = value
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if names.is_empty() {
        vec!["LegeOS".to_owned()]
    } else {
        names
    }
}

pub fn derive_manifest(
    entry: &DesktopEntry,
    desktop_id: &str,
    source: &SourceContext,
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

    let executable_for_provenance = executable.clone();
    let app_id = derive_variant_id(desktop_id, entry.x_flatpak.as_deref());
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
        // These four were parsed and thrown away before: an `AppIdentity1` consumer had no way
        // to draw an icon, group by category, match a keyword, or tell a helper entry from an
        // application a person means to launch.
        icon: entry.icon.clone(),
        categories: entry.categories.iter().cloned().collect(),
        keywords: entry.keywords.iter().cloned().collect(),
        no_display: entry.no_display || hidden_from_desktop(entry, &current_desktops()),
        provenance: Some(provenance_of(
            entry,
            desktop_id,
            source,
            &executable_for_provenance,
        )),
    };
    Some(DerivedManifest {
        manifest,
        exec_template: template,
    })
}

/// What is known about where this copy came from, at the moment the file was read.
///
/// `X-Flatpak` overrides the directory's own classification: a Flatpak export found anywhere is a
/// Flatpak. `declared_privileges` and `provider_claims` are copied verbatim from the entry's
/// `X-LegeOS-*` keys and are inert here — a claim recorded is a claim to *show*, and the code that
/// grants anything reads the grant tables, never this.
fn provenance_of(
    entry: &DesktopEntry,
    desktop_id: &str,
    source: &SourceContext,
    executable: &Path,
) -> AppProvenance {
    let flatpak_id = entry
        .x_flatpak
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let origin = if flatpak_id.is_some() {
        AppOriginKind::Flatpak
    } else {
        source.origin
    };
    AppProvenance {
        source_path: Some(source.path.clone()),
        package_ref: flatpak_id.map(str::to_owned),
        // A desktop entry's `Version=` is the *spec* version, not the application's, so there is
        // nothing honest to put here from a `.desktop` file alone.
        version: None,
        desktop_entry_id: Some(desktop_id.to_owned()),
        launch: if flatpak_id.is_some() {
            LaunchMechanism::FlatpakExport
        } else {
            LaunchMechanism::Direct
        },
        executable: Some(executable.to_path_buf()),
        sandboxed: flatpak_id.is_some(),
        declared_privileges: comma_list(entry.extra.get("X-LegeOS-Background")),
        provider_claims: comma_list(entry.extra.get("X-LegeOS-Provides")),
        logical: entry
            .extra
            .get("X-LegeOS-LogicalApp")
            .and_then(|value| value.trim().parse().ok()),
        ..AppProvenance::from_origin(origin)
    }
}

fn comma_list(value: Option<&String>) -> std::collections::BTreeSet<String> {
    value
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
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

    /// A system application directory: what most of these fixtures are pretending to be.
    fn system_source(desktop_id: &str) -> SourceContext {
        SourceContext::for_file(
            Path::new("/usr/share/applications"),
            PathBuf::from(format!("/usr/share/applications/{desktop_id}")),
        )
    }

    const VIEWER: &str = "\
[Desktop Entry]
Type=Application
Name=Lege Viewer
Exec=lege-viewer --reader %f
TryExec=lege-viewer
MimeType=application/pdf;image/tiff;
Categories=Graphics;Viewer;
Keywords=pdf;document;reader;
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

    /// The real entry this was written for: `/etc/xdg/autostart/lxqt-desktop.desktop` runs
    /// `pcmanfm-qt --desktop`, is `OnlyShowIn=LXQt`, and — being the oldest variant of that
    /// executable — used to be elected the face of PCManFM-Qt on a LegeOS session. The launcher's
    /// only row for it was called "Desktop" and painted a desktop instead of opening a window.
    #[test]
    fn an_entry_for_another_desktop_is_not_displayed_here() {
        let mut lxqt = parse(VIEWER).unwrap();
        lxqt.only_show_in = vec!["LXQt".to_owned()];
        let legeos = ["LegeOS".to_owned()];
        assert!(hidden_from_desktop(&lxqt, &legeos));
        assert!(!hidden_from_desktop(&lxqt, &["LXQt".to_owned()]));

        // NotShowIn is the same rule from the other side, and an entry naming both is excluded.
        let mut not_here = parse(VIEWER).unwrap();
        not_here.not_show_in = vec!["LegeOS".to_owned()];
        assert!(hidden_from_desktop(&not_here, &legeos));
        not_here.only_show_in = vec!["LegeOS".to_owned()];
        assert!(hidden_from_desktop(&not_here, &legeos));

        // An entry naming no desktop shows everywhere, which is nearly every entry there is.
        assert!(!hidden_from_desktop(&parse(VIEWER).unwrap(), &legeos));

        // `XDG_CURRENT_DESKTOP` is colon-separated, and an unset or empty one is read as LegeOS
        // rather than as "every desktop": a session that lost its environment hides other
        // desktops' entries instead of importing all of them as displayable.
        assert_eq!(desktops_from(Some("LegeOS:wlroots")), ["LegeOS", "wlroots"]);
        assert_eq!(desktops_from(None), ["LegeOS"]);
        assert_eq!(desktops_from(Some("  ")), ["LegeOS"]);
    }

    #[test]
    fn hidden_entries_are_skipped_but_no_display_entries_import() {
        let resolver = StaticResolver {
            programs: HashMap::from([("lege-viewer", "/usr/bin/lege-viewer")]),
            flatpaks: HashMap::new(),
        };
        let mut hidden = parse(VIEWER).unwrap();
        hidden.hidden = true;
        assert!(
            derive_manifest(
                &hidden,
                "org.legeos.Viewer.desktop",
                &system_source("org.legeos.Viewer.desktop"),
                &resolver
            )
            .is_none()
        );

        let mut no_display = parse(VIEWER).unwrap();
        no_display.no_display = true;
        let derived = derive_manifest(
            &no_display,
            "org.legeos.Viewer.desktop",
            &system_source("org.legeos.Viewer.desktop"),
            &resolver,
        )
        .unwrap();
        assert!(
            derived.manifest.no_display,
            "the entry imports, but the manifest must remember it is not to be offered"
        );
    }

    #[test]
    fn the_launcher_fields_survive_the_derivation_instead_of_being_parsed_and_dropped() {
        let resolver = StaticResolver {
            programs: HashMap::from([("lege-viewer", "/usr/bin/lege-viewer")]),
            flatpaks: HashMap::new(),
        };
        let entry = parse(VIEWER).unwrap();
        assert_eq!(entry.categories, vec!["Graphics", "Viewer"]);
        assert_eq!(entry.keywords, vec!["pdf", "document", "reader"]);

        let manifest = derive_manifest(
            &entry,
            "org.legeos.Viewer.desktop",
            &system_source("org.legeos.Viewer.desktop"),
            &resolver,
        )
        .unwrap()
        .manifest;
        assert_eq!(manifest.icon.as_deref(), Some("lege-viewer"));
        assert!(manifest.categories.contains("Graphics"));
        assert!(manifest.keywords.contains("reader"));
        assert!(!manifest.no_display);
    }

    #[test]
    fn escaped_keyword_separators_split_on_the_real_semicolons_only() {
        // The spec escapes a literal semicolon inside a list entry as `\;`; unescaping before
        // the split would turn one keyword into two.
        let entry =
            parse("[Desktop Entry]\nType=Application\nName=X\nKeywords=a\\sb;c\\;d;\n").unwrap();
        assert_eq!(entry.keywords, vec!["a b", "c;d"]);
    }

    #[test]
    fn try_exec_gates_import_when_it_fails_to_resolve() {
        let resolver = StaticResolver {
            programs: HashMap::new(),
            flatpaks: HashMap::new(),
        };
        let entry = parse(VIEWER).unwrap();
        assert!(
            derive_manifest(
                &entry,
                "org.legeos.Viewer.desktop",
                &system_source("org.legeos.Viewer.desktop"),
                &resolver
            )
            .is_none()
        );
    }

    #[test]
    fn derives_a_usable_manifest_and_exec_template() {
        let resolver = StaticResolver {
            programs: HashMap::from([("lege-viewer", "/usr/bin/lege-viewer")]),
            flatpaks: HashMap::new(),
        };
        let entry = parse(VIEWER).unwrap();
        let derived = derive_manifest(
            &entry,
            "org.legeos.Viewer.desktop",
            &system_source("org.legeos.Viewer.desktop"),
            &resolver,
        )
        .unwrap();
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
        let derived = derive_manifest(
            &entry,
            "org.mozilla.firefox.desktop",
            &system_source("org.mozilla.firefox.desktop"),
            &resolver,
        )
        .unwrap();
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
        let derived = derive_manifest(
            &entry,
            "prog.desktop",
            &system_source("prog.desktop"),
            &resolver,
        )
        .unwrap();
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

    #[test]
    fn a_flatpak_export_is_keyed_on_its_application_id_not_on_the_desktop_file() {
        // The whole point of D3: `firefox.desktop` from Flathub and `firefox.desktop` from the
        // distribution must not derive the same id, or one of them ceases to exist.
        assert_eq!(
            derive_variant_id("firefox.desktop", Some("org.mozilla.firefox")).as_str(),
            "flatpak.org.mozilla.firefox"
        );
        assert_eq!(
            derive_variant_id("firefox.desktop", None).as_str(),
            "desktop-import.firefox.desktop"
        );
        // An empty X-Flatpak= is not a Flatpak id; it is a malformed line.
        assert_eq!(
            derive_variant_id("firefox.desktop", Some("  ")).as_str(),
            "desktop-import.firefox.desktop"
        );
    }

    #[test]
    fn directories_classify_into_origins_and_namespaces() {
        use lege_types::AppOriginKind;
        assert_eq!(
            SourceContext::classify_directory(Path::new("/usr/share/applications")),
            (OriginNamespace::Xdg, AppOriginKind::DistroPackage)
        );
        assert_eq!(
            SourceContext::classify_directory(Path::new(
                "/var/lib/flatpak/exports/share/applications"
            )),
            (OriginNamespace::Flatpak, AppOriginKind::Flatpak)
        );
        assert_eq!(
            SourceContext::classify_directory(Path::new("/home/person/.local/share/applications")),
            (OriginNamespace::Xdg, AppOriginKind::UserDesktopEntry)
        );
        assert_eq!(
            SourceContext::classify_directory(Path::new("/etc/xdg/autostart")),
            (OriginNamespace::Xdg, AppOriginKind::DistroPackage)
        );
        // Somewhere the system knows nothing about: unknown, and unknown is untrusted.
        assert_eq!(
            SourceContext::classify_directory(Path::new("/tmp/whatever")),
            (OriginNamespace::Xdg, AppOriginKind::Unknown)
        );
    }

    #[test]
    fn provenance_records_the_origin_and_derives_trust_from_it() {
        use lege_types::{AppOriginKind, LaunchMechanism, TrustTier, UpdateOwner};
        let resolver = StaticResolver {
            programs: HashMap::from([("lege-viewer", "/usr/bin/lege-viewer")]),
            flatpaks: HashMap::new(),
        };
        let entry = parse(VIEWER).unwrap();

        let packaged = derive_manifest(
            &entry,
            "org.legeos.Viewer.desktop",
            &system_source("org.legeos.Viewer.desktop"),
            &resolver,
        )
        .unwrap()
        .manifest;
        let provenance = packaged.provenance.clone().unwrap();
        assert_eq!(provenance.origin, AppOriginKind::DistroPackage);
        assert_eq!(provenance.launch, LaunchMechanism::Direct);
        assert_eq!(provenance.update_owner, UpdateOwner::Distro);
        assert_eq!(
            provenance.desktop_entry_id.as_deref(),
            Some("org.legeos.Viewer.desktop")
        );
        assert!(!provenance.sandboxed);
        assert_eq!(packaged.trust(), TrustTier::System);

        let mine = derive_manifest(
            &entry,
            "org.legeos.Viewer.desktop",
            &SourceContext::for_file(
                Path::new("/home/person/.local/share/applications"),
                PathBuf::from("/home/person/.local/share/applications/org.legeos.Viewer.desktop"),
            ),
            &resolver,
        )
        .unwrap()
        .manifest;
        assert_eq!(
            mine.provenance.as_ref().unwrap().origin,
            AppOriginKind::UserDesktopEntry
        );
        assert_eq!(
            mine.trust(),
            TrustTier::User,
            "a file the account can rewrite must not read as the system's"
        );
    }

    #[test]
    fn a_flatpak_export_reads_as_a_flatpak_wherever_the_file_sits() {
        use lege_types::{AppOriginKind, LaunchMechanism, TrustTier, UpdateOwner};
        let resolver = StaticResolver {
            programs: HashMap::new(),
            flatpaks: HashMap::from([(
                "org.mozilla.firefox",
                "/var/lib/flatpak/exports/bin/org.mozilla.firefox",
            )]),
        };
        let text = "[Desktop Entry]\nType=Application\nName=Firefox\nExec=/usr/bin/flatpak run org.mozilla.firefox %u\nX-Flatpak=org.mozilla.firefox\n";
        let entry = parse(text).unwrap();
        let manifest = derive_manifest(
            &entry,
            "firefox.desktop",
            // Deliberately classified as a plain system directory: X-Flatpak= overrides it.
            &system_source("firefox.desktop"),
            &resolver,
        )
        .unwrap()
        .manifest;
        assert_eq!(manifest.id.as_str(), "flatpak.org.mozilla.firefox");
        let provenance = manifest.provenance.clone().unwrap();
        assert_eq!(provenance.origin, AppOriginKind::Flatpak);
        assert_eq!(provenance.launch, LaunchMechanism::FlatpakExport);
        assert_eq!(provenance.update_owner, UpdateOwner::Flatpak);
        assert_eq!(
            provenance.package_ref.as_deref(),
            Some("org.mozilla.firefox")
        );
        assert!(provenance.sandboxed);
        assert_eq!(manifest.trust(), TrustTier::System);
    }

    #[test]
    fn declared_privileges_and_provider_claims_are_recorded_as_claims() {
        let resolver = StaticResolver {
            programs: HashMap::from([("sync-agent", "/usr/bin/sync-agent")]),
            flatpaks: HashMap::new(),
        };
        let text = "[Desktop Entry]\nType=Application\nName=Sync\nExec=sync-agent\nX-LegeOS-Background=continuous-background,network-while-hidden\nX-LegeOS-Provides=notification-server\nX-LegeOS-LogicalApp=logical.sync\n";
        let entry = parse(text).unwrap();
        let manifest = derive_manifest(
            &entry,
            "sync.desktop",
            &system_source("sync.desktop"),
            &resolver,
        )
        .unwrap()
        .manifest;
        let provenance = manifest.provenance.clone().unwrap();
        assert!(
            provenance
                .declared_privileges
                .contains("continuous-background")
        );
        assert!(
            provenance
                .declared_privileges
                .contains("network-while-hidden")
        );
        assert!(provenance.provider_claims.contains("notification-server"));
        assert_eq!(
            manifest.declared_logical_app().map(ToString::to_string),
            Some("logical.sync".to_owned()),
        );
    }
}
