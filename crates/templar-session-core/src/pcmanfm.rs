//! Repair of the two PCManFM-Qt settings that otherwise break it inside a TemplarOS session.
//!
//! PCManFM-Qt is the session's file manager (`docs/architecture/files.md`), and it carries two
//! built-in defaults that do not exist on a TemplarOS machine: `[System] Terminal=xterm` and
//! `[System] FallbackIconThemeName=oxygen`. The second matters because nothing in this session
//! gives Qt an icon theme, so PCManFM-Qt falls back to the one it names; when that theme is not
//! installed, every file and folder in the icon view is drawn as text alone.
//!
//! A fresh account gets the shipped `/etc/xdg/pcmanfm-qt/default/settings.conf`, which PCManFM-Qt
//! reads only while `$XDG_CONFIG_HOME/pcmanfm-qt/default/` does not exist. An account that already
//! has its own profile keeps whatever it last saved, so `templar-sessiond` repairs that file at
//! startup — before `Launcher1` is claimed, so before anything can start PCManFM-Qt — and
//! `templar pcmanfm repair` runs the same code on demand.
//!
//! The rule is narrow on purpose: a key is rewritten only when what it names cannot work here (a
//! terminal whose executable is not on `PATH`, an icon theme with no `index.theme` in any icon
//! directory). A user who chose `foot` or `breeze` keeps it. Every other byte of the file —
//! other keys, other sections, comments, line endings — is left exactly as it was.
//!
//! Alongside it, [`ensure_terminal_entry`] gives libfm-qt the one terminal-list group it needs to
//! run a command in `templar-terminal` rather than in `xterm`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    ffi::OsString,
    fs,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// The terminal command TemplarOS gives PCManFM-Qt. `templar-terminal` (shipped by
/// `templaros-cli`) resolves the Terminal provider role and starts it through `Launcher1`, so the
/// terminal gets its own application unit and identity instead of landing in PCManFM-Qt's cgroup.
pub const TERMINAL: &str = "templar-terminal";

/// Icon themes PCManFM-Qt may fall back to, in preference order; the first installed one wins.
/// Papirus is what `templaros-desktop` installs and the one theme here with a picture for every
/// freedesktop name PCManFM-Qt asks for, toolbar actions included. Yaru and Adwaita follow the
/// shell's own lookup order (`templar-shell/src/icons.rs`); Adwaita alone lacks most full-colour
/// action icons, which is why it does not lead. `hicolor` is the specified last resort.
pub const ICON_THEMES: [&str; 4] = ["Papirus", "Yaru", "Adwaita", "hicolor"];

const SECTION: &str = "System";
const TERMINAL_KEY: &str = "Terminal";
const ICON_THEME_KEY: &str = "FallbackIconThemeName";
/// What PCManFM-Qt itself uses when the key is absent (`pcmanfm/settings.cpp`, `loadFile`).
const BUILTIN_TERMINAL: &str = "xterm";
/// What PCManFM-Qt itself uses when the key is absent or empty.
const BUILTIN_ICON_THEME: &str = "oxygen";

static TEMP: AtomicU64 = AtomicU64::new(0);

/// `$XDG_CONFIG_HOME/pcmanfm-qt/default/settings.conf` — the profile PCManFM-Qt starts with.
pub fn settings_path(config_home: &Path) -> PathBuf {
    config_home.join("pcmanfm-qt/default/settings.conf")
}

/// The machine facts the repair judges values against.
#[derive(Clone, Debug, Default)]
pub struct Host {
    /// `PATH`, split.
    pub path: Vec<PathBuf>,
    /// Icon-theme base directories, searched for `<theme>/index.theme`.
    pub icon_dirs: Vec<PathBuf>,
}

impl Host {
    /// `PATH`, and the icon directories Qt searches: `$XDG_DATA_HOME/icons`, `~/.icons`, and
    /// `icons/` under every `$XDG_DATA_DIRS` entry. In `templar-sessiond` this `PATH` is the
    /// session user manager's, which is also the one a `Launcher1`-started PCManFM-Qt spawns its
    /// terminal with.
    pub fn from_env() -> Self {
        let path = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut icon_dirs = Vec::new();
        match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            Some(data_home) => icon_dirs.push(PathBuf::from(data_home).join("icons")),
            None => icon_dirs.extend(home.iter().map(|home| home.join(".local/share/icons"))),
        }
        icon_dirs.extend(home.iter().map(|home| home.join(".icons")));
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
        icon_dirs.extend(std::env::split_paths(&data_dirs).map(|dir| dir.join("icons")));
        Self { path, icon_dirs }
    }

    /// Whether the program a `Terminal=` value starts exists. libfm-qt runs the value through
    /// `g_shell_parse_argv` and spawns with `G_SPAWN_SEARCH_PATH`, so the first word is the
    /// program: a path is checked as a path, a bare name against `PATH`.
    fn terminal_exists(&self, value: &str) -> bool {
        let Some(program) = value
            .split_whitespace()
            .next()
            .map(|word| word.trim_matches(|c| c == '"' || c == '\''))
            .filter(|word| !word.is_empty())
        else {
            return false;
        };
        if program.contains('/') {
            return is_executable(Path::new(program));
        }
        self.path.iter().any(|dir| is_executable(&dir.join(program)))
    }

    fn icon_theme_installed(&self, name: &str) -> bool {
        !name.is_empty()
            && !name.contains('/')
            && name != "."
            && name != ".."
            && self
                .icon_dirs
                .iter()
                .any(|dir| dir.join(name).join("index.theme").is_file())
    }

    /// The first of [`ICON_THEMES`] that is installed, if any is.
    pub fn default_icon_theme(&self) -> Option<&'static str> {
        ICON_THEMES
            .into_iter()
            .find(|theme| self.icon_theme_installed(theme))
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// One rewritten key. `before` is `None` when the key was absent and PCManFM-Qt's built-in
/// default (xterm, oxygen) was the thing that could not work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Repair {
    pub key: &'static str,
    pub before: Option<String>,
    pub after: String,
}

/// What [`repair_file`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum Outcome {
    /// The profile directory does not exist: PCManFM-Qt reads the shipped system default, and
    /// there is nothing of the user's to repair.
    NoProfile,
    /// Every value can work here; the file was not touched.
    Unchanged,
    /// These keys were rewritten (or added); nothing else changed.
    Repaired { repairs: Vec<Repair> },
}

/// One `[System]` key line: its index and its (unquoted) value.
struct Occurrence {
    line: usize,
    value: String,
}

/// Where the two keys are in a document.
#[derive(Default)]
struct Scan {
    terminal: Vec<Occurrence>,
    icon_theme: Vec<Occurrence>,
    /// The last non-blank line of the last `[System]` section: where a missing key is added.
    insert_after: Option<usize>,
}

fn scan(lines: &[&str]) -> Scan {
    let mut found = Scan::default();
    let mut in_system = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            in_system = name.trim() == SECTION;
            if in_system {
                found.insert_after = Some(index);
            }
            continue;
        }
        if !in_system || trimmed.is_empty() {
            continue;
        }
        found.insert_after = Some(index);
        if trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim()).to_owned();
        match key.trim() {
            TERMINAL_KEY => found.terminal.push(Occurrence { line: index, value }),
            ICON_THEME_KEY => found.icon_theme.push(Occurrence { line: index, value }),
            _ => {}
        }
    }
    found
}

/// Repair `text`, an INI document as QSettings writes it. Returns the new document and what
/// changed; when nothing needs repair the document is returned unchanged, byte for byte.
pub fn repair_text(text: &str, host: &Host) -> (String, Vec<Repair>) {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let found = scan(&lines);
    let mut repairs = Vec::new();
    // QSettings reads the last occurrence of a duplicated key. A value that already is the
    // default is left alone even when it does not resolve (templaros-cli not installed yet):
    // rewriting it to itself would change nothing.
    let current_terminal = found.terminal.last().map(|found| found.value.clone());
    let terminal = current_terminal.as_deref().unwrap_or(BUILTIN_TERMINAL);
    if terminal != TERMINAL && !host.terminal_exists(terminal) {
        repairs.push((
            &found.terminal,
            Repair {
                key: TERMINAL_KEY,
                before: current_terminal,
                after: TERMINAL.to_owned(),
            },
        ));
    }
    let current_icon_theme = found.icon_theme.last().map(|found| found.value.clone());
    let effective = current_icon_theme
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(BUILTIN_ICON_THEME);
    if !host.icon_theme_installed(effective)
        && let Some(theme) = host.default_icon_theme()
    {
        repairs.push((
            &found.icon_theme,
            Repair {
                key: ICON_THEME_KEY,
                before: current_icon_theme,
                after: theme.to_owned(),
            },
        ));
    }
    if repairs.is_empty() {
        return (text.to_owned(), Vec::new());
    }
    let out = render(text, &lines, &repairs, found.insert_after);
    (out, repairs.into_iter().map(|(_, repair)| repair).collect())
}

/// `text` with every occurrence of a repaired key rewritten in place (its line ending kept) and
/// absent keys added after `insert_after`, or in a new `[System]` section at the end.
fn render(
    text: &str,
    lines: &[&str],
    repairs: &[(&Vec<Occurrence>, Repair)],
    insert_after: Option<usize>,
) -> String {
    let mut replaced: Vec<Option<String>> = vec![None; lines.len()];
    let mut added = Vec::new();
    for (occurrences, repair) in repairs {
        let assignment = format!("{}={}", repair.key, repair.after);
        if occurrences.is_empty() {
            added.push(assignment);
            continue;
        }
        for occurrence in *occurrences {
            let line = lines[occurrence.line];
            let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
            replaced[occurrence.line] = Some(format!("{assignment}{ending}"));
        }
    }

    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(text.len() + 64);
    for (index, line) in lines.iter().enumerate() {
        out.push_str(replaced[index].as_deref().unwrap_or(line));
        if Some(index) == insert_after && !added.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(newline);
            }
            for assignment in added.drain(..) {
                out.push_str(&assignment);
                out.push_str(newline);
            }
        }
    }
    if !added.is_empty() {
        // No [System] section at all: add one at the end.
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push_str(newline);
            }
            out.push_str(newline);
        }
        out.push('[');
        out.push_str(SECTION);
        out.push(']');
        out.push_str(newline);
        for assignment in added {
            out.push_str(&assignment);
            out.push_str(newline);
        }
    }
    out
}

/// QSettings quotes a string value that needs it; the repair reads past the quotes.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value)
}

/// Repair the settings file at `path` in place.
///
/// A missing file inside an existing profile directory is created with just the repaired keys,
/// because PCManFM-Qt would otherwise run on its built-in xterm/oxygen defaults; a missing
/// profile directory is left alone (PCManFM-Qt reads the shipped system default instead).
/// With `backup`, the original bytes are first copied there; an existing backup is never
/// overwritten. The rewrite is atomic, and it is abandoned if the file changes while it is being
/// prepared (PCManFM-Qt saves its settings on exit).
pub fn repair_file(path: &Path, host: &Host, backup: Option<&Path>) -> Result<Outcome> {
    let (target, original) = read_through_links(path)?;
    let Some(directory) = target.parent().filter(|dir| dir.is_dir()) else {
        return Ok(Outcome::NoProfile);
    };
    let text = match &original {
        Some(bytes) => std::str::from_utf8(bytes)
            .with_context(|| format!("{} is not UTF-8; left untouched", target.display()))?,
        None => "",
    };
    let (repaired, repairs) = repair_text(text, host);
    if repairs.is_empty() {
        return Ok(Outcome::Unchanged);
    }
    if let (Some(backup), Some(bytes)) = (backup, &original) {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(backup)
            .with_context(|| {
                format!("creating the backup {} (an existing one is never overwritten)", backup.display())
            })?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    publish(&target, directory, original.as_deref(), &repaired)?;
    Ok(Outcome::Repaired { repairs })
}

/// `$XDG_DATA_HOME/libfm-qt/terminals.list` — the per-user terminal list libfm-qt consults
/// before its own `/usr/share/libfm-qt6/terminals.list`.
pub fn terminals_list_path(data_home: &Path) -> PathBuf {
    data_home.join("libfm-qt/terminals.list")
}

/// How libfm-qt runs a command in [`TERMINAL`] ("Execute in Terminal", `Terminal=true`
/// applications): `templar-terminal -e CMD`, or `--hold -e CMD` to keep the window open.
///
/// libfm-qt (`src/core/terminal.cpp`, `expand_terminal`) builds that command line only for a
/// terminal it finds a group for, in the first `libfm-qt/terminals.list` under the XDG data
/// directories or in its own list; for any other name it runs `xterm -e CMD`, whatever the
/// setting says. A system-wide copy cannot be shipped: `/usr/share/libfm-qt/terminals.list` is
/// `libfm-qt5-common`'s file. So the group goes in the per-user list. It is inert unless
/// `Terminal=templar-terminal`, and other groups in the file still resolve, because libfm-qt
/// falls back to its own list for any key the user list does not carry.
const TERMINAL_ENTRY: &str = "[templar-terminal]\nopen_arg=-e\nnoclose_arg=--hold -e\n";

/// `text` with the [`TERMINAL`] group appended, or `None` if it already has one (whatever it
/// says: a group the user edited is theirs).
pub fn terminal_entry_text(text: &str) -> Option<String> {
    let header = format!("[{TERMINAL}]");
    if text.lines().any(|line| line.trim() == header) {
        return None;
    }
    let mut out = text.to_owned();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(TERMINAL_ENTRY);
    Some(out)
}

/// Add the [`TERMINAL`] group to the per-user list at `path`, creating the file (and its
/// directory) if needed. Returns whether anything was written.
pub fn ensure_terminal_entry(path: &Path) -> Result<bool> {
    let (target, original) = read_through_links(path)?;
    let text = match &original {
        Some(bytes) => std::str::from_utf8(bytes)
            .with_context(|| format!("{} is not UTF-8; left untouched", target.display()))?,
        None => "",
    };
    let Some(contents) = terminal_entry_text(text) else {
        return Ok(false);
    };
    let directory = target.parent().context("the terminal list has no parent directory")?;
    fs::create_dir_all(directory).with_context(|| format!("creating {}", directory.display()))?;
    publish(&target, directory, original.as_deref(), &contents)?;
    Ok(true)
}

/// The file a path finally names (a symlinked config is repaired where it lives, and the link
/// kept), and its bytes if it exists.
fn read_through_links(path: &Path) -> Result<(PathBuf, Option<Vec<u8>>)> {
    let target = match fs::canonicalize(path) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_owned(),
        Err(error) => return Err(error).with_context(|| format!("resolving {}", path.display())),
    };
    match fs::read(&target) {
        Ok(bytes) => Ok((target, Some(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((target, None)),
        Err(error) => Err(error).with_context(|| format!("reading {}", target.display())),
    }
}

/// Write `contents` to `target` through a temporary file in the same directory, refusing if the
/// file no longer holds `original` (or, for a new file, if one has appeared).
fn publish(target: &Path, directory: &Path, original: Option<&[u8]>, contents: &str) -> Result<()> {
    let name = target.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    let temp = directory.join(format!(
        ".{name}.templaros-{}-{}.tmp",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("creating {}", temp.display()))?;
        if original.is_some() {
            let mode = fs::metadata(target)?.permissions().mode();
            fs::set_permissions(&temp, fs::Permissions::from_mode(mode & 0o7777))?;
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        match original {
            Some(original) => {
                let now = fs::read(target).with_context(|| format!("re-reading {}", target.display()))?;
                anyhow::ensure!(
                    now == original,
                    "{} changed while it was being repaired; left as it is now",
                    target.display()
                );
                fs::rename(&temp, target)
                    .with_context(|| format!("replacing {}", target.display()))?;
            }
            // A hard link never replaces: a file created meanwhile is kept, not overwritten.
            None => fs::hard_link(&temp, target)
                .with_context(|| format!("creating {}", target.display()))?,
        }
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with an executable `kitty`, `foot` and `templar-terminal` on `PATH` and the
    /// Papirus and breeze icon themes installed.
    struct Fixture {
        _root: tempfile::TempDir,
        host: Host,
        profile: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["kitty", "foot", TERMINAL] {
            let program = bin.join(name);
            fs::write(&program, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Present but not executable: not a terminal anybody can start.
        fs::write(bin.join("xterm"), "").unwrap();
        let icons = root.path().join("icons");
        for theme in ["Papirus", "breeze"] {
            fs::create_dir_all(icons.join(theme)).unwrap();
            fs::write(icons.join(theme).join("index.theme"), "[Icon Theme]\n").unwrap();
        }
        // A directory without index.theme is not a theme.
        fs::create_dir_all(icons.join("oxygen")).unwrap();
        let profile = root.path().join("config/pcmanfm-qt/default");
        fs::create_dir_all(&profile).unwrap();
        Fixture {
            host: Host { path: vec![bin], icon_dirs: vec![icons] },
            profile,
            _root: root,
        }
    }

    const BROKEN: &str = "[Behavior]\r\nConfirmDelete=true\r\n\r\n[System]\r\nArchiver=file-roller\r\nFallbackIconThemeName=oxygen\r\nSIUnit=false\r\nSuCommand=lxqt-sudo %s\r\nTerminal=xterm\r\n\r\n[Window]\r\nFixedHeight=480\r\n";

    #[test]
    fn invalid_values_are_repaired_and_every_other_byte_is_kept() {
        let f = fixture();
        let (out, repairs) = repair_text(BROKEN, &f.host);
        assert_eq!(
            out,
            BROKEN
                .replace("FallbackIconThemeName=oxygen", "FallbackIconThemeName=Papirus")
                .replace("Terminal=xterm", "Terminal=templar-terminal")
        );
        assert_eq!(
            repairs,
            vec![
                Repair { key: "Terminal", before: Some("xterm".into()), after: TERMINAL.into() },
                Repair {
                    key: "FallbackIconThemeName",
                    before: Some("oxygen".into()),
                    after: "Papirus".into()
                },
            ]
        );
    }

    #[test]
    fn valid_user_choices_are_left_alone() {
        let f = fixture();
        let text = "[System]\nFallbackIconThemeName=breeze\nTerminal=foot --server\n\n[Window]\nX=1";
        let (out, repairs) = repair_text(text, &f.host);
        assert!(repairs.is_empty());
        assert_eq!(out, text);
        // An absolute path to an executable is as good as a name on PATH.
        let absolute = format!("[System]\nFallbackIconThemeName=Papirus\nTerminal={}\n", f.host.path[0].join("kitty").display());
        assert!(repair_text(&absolute, &f.host).1.is_empty());
    }

    #[test]
    fn a_missing_key_is_added_to_the_system_section_only() {
        let f = fixture();
        let text = "[System]\nArchiver=ark\n\n[Window]\nFixedHeight=480\n";
        let (out, repairs) = repair_text(text, &f.host);
        assert_eq!(
            out,
            "[System]\nArchiver=ark\nTerminal=templar-terminal\nFallbackIconThemeName=Papirus\n\n[Window]\nFixedHeight=480\n"
        );
        assert_eq!(repairs[0].before, None, "the built-in xterm was what failed");
        assert_eq!(repairs[1].before, None, "the built-in oxygen was what failed");
    }

    #[test]
    fn a_missing_system_section_is_appended() {
        let f = fixture();
        let text = "[Window]\nFixedHeight=480";
        let (out, _) = repair_text(text, &f.host);
        assert_eq!(
            out,
            "[Window]\nFixedHeight=480\n\n[System]\nTerminal=templar-terminal\nFallbackIconThemeName=Papirus\n"
        );
    }

    #[test]
    fn keys_of_the_same_name_in_other_sections_are_not_touched() {
        let f = fixture();
        let text = "[Other]\nTerminal=xterm\n\n[System]\nFallbackIconThemeName=Papirus\nTerminal=\"xterm\"\n";
        let (out, repairs) = repair_text(text, &f.host);
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].before.as_deref(), Some("xterm"), "QSettings quotes are read past");
        assert_eq!(
            out,
            "[Other]\nTerminal=xterm\n\n[System]\nFallbackIconThemeName=Papirus\nTerminal=templar-terminal\n"
        );
    }

    #[test]
    fn an_empty_value_cannot_work_and_is_repaired() {
        let f = fixture();
        let (out, repairs) = repair_text("[System]\nFallbackIconThemeName=\nTerminal=\n", &f.host);
        assert_eq!(repairs.len(), 2);
        assert_eq!(out, "[System]\nFallbackIconThemeName=Papirus\nTerminal=templar-terminal\n");
    }

    #[test]
    fn the_default_terminal_is_not_rewritten_to_itself_before_it_is_installed() {
        let mut f = fixture();
        f.host.path.clear();
        let text = "[System]\nFallbackIconThemeName=Papirus\nTerminal=templar-terminal\n";
        let (out, repairs) = repair_text(text, &f.host);
        assert!(repairs.is_empty());
        assert_eq!(out, text);
    }

    #[test]
    fn no_installed_fallback_theme_leaves_the_icon_key_alone() {
        let mut f = fixture();
        f.host.icon_dirs.clear();
        let (out, repairs) = repair_text("[System]\nFallbackIconThemeName=oxygen\nTerminal=kitty\n", &f.host);
        assert!(repairs.is_empty());
        assert_eq!(out, "[System]\nFallbackIconThemeName=oxygen\nTerminal=kitty\n");
    }

    #[test]
    fn the_file_is_rewritten_with_a_backup_and_its_mode_kept() {
        let f = fixture();
        let path = f.profile.join("settings.conf");
        fs::write(&path, BROKEN).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let backup = f.profile.join("settings.conf.templaros-bak");
        let outcome = repair_file(&path, &f.host, Some(&backup)).unwrap();
        assert!(matches!(outcome, Outcome::Repaired { ref repairs } if repairs.len() == 2));
        assert_eq!(fs::read_to_string(&backup).unwrap(), BROKEN);
        let repaired = fs::read_to_string(&path).unwrap();
        assert!(repaired.contains("Terminal=templar-terminal\r\n"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // Nothing left behind but the file and its backup.
        assert_eq!(fs::read_dir(&f.profile).unwrap().count(), 2);
        // Run again: already valid, untouched, and the existing backup is not needed.
        assert_eq!(repair_file(&path, &f.host, Some(&backup)).unwrap(), Outcome::Unchanged);
        assert_eq!(fs::read_to_string(&path).unwrap(), repaired);
    }

    #[test]
    fn an_existing_backup_is_never_overwritten() {
        let f = fixture();
        let path = f.profile.join("settings.conf");
        fs::write(&path, BROKEN).unwrap();
        let backup = f.profile.join("settings.conf.templaros-bak");
        fs::write(&backup, "older").unwrap();
        assert!(repair_file(&path, &f.host, Some(&backup)).is_err());
        assert_eq!(fs::read_to_string(&backup).unwrap(), "older");
        assert_eq!(fs::read_to_string(&path).unwrap(), BROKEN, "no backup, no rewrite");
    }

    #[test]
    fn a_missing_file_in_an_existing_profile_is_created_with_the_repaired_keys() {
        let f = fixture();
        let path = f.profile.join("settings.conf");
        let outcome = repair_file(&path, &f.host, None).unwrap();
        assert!(matches!(outcome, Outcome::Repaired { .. }));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[System]\nTerminal=templar-terminal\nFallbackIconThemeName=Papirus\n"
        );
    }

    #[test]
    fn a_missing_profile_is_left_to_the_system_default() {
        let f = fixture();
        let path = f.profile.join("../../absent/default/settings.conf");
        assert_eq!(repair_file(&path, &f.host, None).unwrap(), Outcome::NoProfile);
        assert!(!path.exists());
    }

    #[test]
    fn a_symlinked_settings_file_is_repaired_through_the_link() {
        let f = fixture();
        let real = f.profile.join("real.conf");
        fs::write(&real, "[System]\nTerminal=xterm\nFallbackIconThemeName=Papirus\n").unwrap();
        let link = f.profile.join("settings.conf");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        repair_file(&link, &f.host, None).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "[System]\nTerminal=templar-terminal\nFallbackIconThemeName=Papirus\n"
        );
    }

    #[test]
    fn the_terminal_group_is_appended_once_and_other_groups_kept() {
        let user = "[foot]\nopen_arg=-e\ncustom_args=--server";
        let out = terminal_entry_text(user).unwrap();
        assert_eq!(out, format!("{user}\n\n{TERMINAL_ENTRY}"));
        assert_eq!(terminal_entry_text(&out), None);
        // A group the user already edited is theirs.
        assert_eq!(terminal_entry_text("[templar-terminal]\nopen_arg=-x\n"), None);
        assert_eq!(terminal_entry_text("").unwrap(), TERMINAL_ENTRY);
    }

    #[test]
    fn the_per_user_terminal_list_is_created_with_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = terminals_list_path(root.path());
        assert!(ensure_terminal_entry(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), TERMINAL_ENTRY);
        assert!(!ensure_terminal_entry(&path).unwrap(), "already present: untouched");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }
}
