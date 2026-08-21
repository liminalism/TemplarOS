//! Session discovery: `Name=`/`Exec=` out of `/usr/share/wayland-sessions/*.desktop` and
//! `/usr/share/xsessions/*.desktop` — the same two directories every other greeter reads, and
//! where `legeos.desktop` (`session/units/wayland-sessions/legeos.desktop`) is installed
//! (`debian/legeos-session.install`). Minimal freedesktop.org Desktop Entry parsing: only the
//! `[Desktop Entry]` group, only the unlocalized `Name=`/`Exec=`/`Hidden=`/`NoDisplay=` keys — a
//! login picker has no locale negotiated yet and needs nothing else these files carry.
//!
//! Pure functions throughout (`parse_desktop_file`, `split_exec`, `default_index`); the only
//! filesystem-touching function is [`discover`], which is a thin, individually-fallible walk over
//! them so one unreadable or malformed file cannot hide every other session — see its doc.

use std::path::{Path, PathBuf};

/// Which of the two picker directories a session came from — `start_session`'s caller
/// (`crate::surface::app`) needs this to set `XDG_SESSION_TYPE` correctly, since a greeter that
/// only ever reads `/usr/share/wayland-sessions` is not what the task asked for: the xsessions
/// directory is explicitly in scope too, and starting an X11 session while claiming
/// `XDG_SESSION_TYPE=wayland` would be a real, if quiet, correctness bug for exactly the sessions
/// this crate goes out of its way to discover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Wayland,
    X11,
}

/// One session a user can log into: what to show, and what to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    /// Already split into an argv — `greetd-ipc(7)`'s `start_session` takes `cmd: Vec<String>`,
    /// not a shell string.
    pub exec: Vec<String>,
    pub kind: SessionKind,
    /// Where this came from, for diagnostics only — never shown to the user, never fed back into
    /// anything that runs.
    pub source: PathBuf,
}

/// The two directories every greeter reads sessions from, paired with what starting one of them
/// means for `XDG_SESSION_TYPE`.
pub fn default_dirs() -> Vec<(SessionKind, PathBuf)> {
    vec![
        (
            SessionKind::Wayland,
            PathBuf::from("/usr/share/wayland-sessions"),
        ),
        (SessionKind::X11, PathBuf::from("/usr/share/xsessions")),
    ]
}

/// Read every `*.desktop` file under each of `dirs` (non-recursively, matching every greeter's own
/// convention) and parse the usable ones. A directory that does not exist is skipped rather than
/// an error — `/usr/share/xsessions` legitimately does not exist on a Wayland-only install — and a
/// single unreadable or malformed file is logged and skipped rather than aborting the whole scan,
/// so one broken third-party `.desktop` file cannot hide every session, including LegeOS's own.
pub fn discover(dirs: &[(SessionKind, PathBuf)]) -> Vec<SessionEntry> {
    let mut entries = Vec::new();
    for (kind, dir) in dirs {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = read_dir
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "desktop"))
            .collect();
        // Deterministic order — both for a stable on-screen list and for `default_index`'s "first
        // match wins" tie-break to mean the same thing on every run.
        paths.sort();
        for path in paths {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "could not read session file");
                    continue;
                }
            };
            if let Some(entry) = parse_desktop_file(&text, *kind, &path) {
                entries.push(entry);
            } else {
                tracing::warn!(
                    path = %path.display(),
                    "skipping session file: no usable Name/Exec, or Hidden/NoDisplay"
                );
            }
        }
    }
    entries
}

/// Parse one `.desktop` file's `[Desktop Entry]` group. `None` if it has no usable `Name`/`Exec`
/// pair, or declares itself `Hidden`/`NoDisplay` — the same "do not show this" convention every
/// desktop environment's own application/session pickers honour.
pub fn parse_desktop_file(text: &str, kind: SessionKind, source: &Path) -> Option<SessionEntry> {
    let mut in_desktop_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut hidden = false;
    let mut no_display = false;

    for line in text.lines() {
        let line = line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_desktop_entry = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // Only the exact, unlocalized key — `Name[fr]=`/`Comment[de]=` and the like are simply
        // not this key, so they fall through untouched rather than needing to be excluded.
        match key.trim() {
            "Name" => name = Some(value.trim().to_owned()),
            "Exec" => exec = Some(value.trim().to_owned()),
            "Hidden" => hidden = value.trim().eq_ignore_ascii_case("true"),
            "NoDisplay" => no_display = value.trim().eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    if hidden || no_display {
        return None;
    }
    let name = name?;
    let exec = split_exec(&exec?);
    if name.is_empty() || exec.is_empty() {
        return None;
    }
    Some(SessionEntry {
        name,
        exec,
        kind,
        source: source.to_owned(),
    })
}

/// A minimal Desktop Entry `Exec=` tokenizer: splits on whitespace, honouring double-quoted
/// arguments (the only quoting form the spec defines for this key) with `\"`/`\\` escapes inside
/// them, and drops the standalone field-code tokens (`%f %F %u %U %d %D %n %N %i %c %k %v %m`) a
/// session launcher never supplies — a greeter starts a session with no file, URL or icon
/// argument. `%%` unescapes to one literal `%`.
fn split_exec(exec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quotes = false;
    let mut chars = exec.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '%' => match chars.peek() {
                Some('%') => {
                    current.push('%');
                    has_token = true;
                    chars.next();
                }
                Some(_) => {
                    // Drop the field-code letter and contribute nothing — a field code standing
                    // alone as its own word (the common case: `app %f`) must vanish entirely
                    // rather than leave a bare empty-string argument behind.
                    chars.next();
                }
                None => {
                    current.push('%');
                    has_token = true;
                }
            },
            character if character.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            character => {
                current.push(character);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    tokens
}

/// Where the default selection should land: the entry named exactly "LegeOS" if one exists
/// (`docs/adr/0010-session-greeter.md`), else the first entry, else `0` for an empty list — the
/// screen is responsible for treating an empty list as "no sessions found" rather than indexing
/// into it.
pub fn default_index(entries: &[SessionEntry]) -> usize {
    entries
        .iter()
        .position(|entry| entry.name == "LegeOS")
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path() -> PathBuf {
        PathBuf::from("/usr/share/wayland-sessions/test.desktop")
    }

    #[test]
    fn a_simple_entry_parses() {
        let text = "[Desktop Entry]\nName=LegeOS\nComment=LegeOS desktop session\nExec=lege-session\nType=Application\nDesktopNames=LegeOS\n";
        let entry = parse_desktop_file(text, SessionKind::Wayland, &path()).unwrap();
        assert_eq!(entry.name, "LegeOS");
        assert_eq!(entry.exec, vec!["lege-session"]);
    }

    /// The real installed file this crate is going to see in production —
    /// `session/units/wayland-sessions/legeos.desktop`, verbatim.
    #[test]
    fn the_actual_legeos_desktop_file_parses() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../session/units/wayland-sessions/legeos.desktop"
        ))
        .expect("session/units/wayland-sessions/legeos.desktop must exist and be readable");
        let entry = parse_desktop_file(&text, SessionKind::Wayland, &path()).unwrap();
        assert_eq!(entry.name, "LegeOS");
        assert_eq!(entry.exec, vec!["lege-session"]);
    }

    /// A real, messy file with dozens of localized `Name[xx]=`/`Comment[xx]=` lines interleaved
    /// with the unlocalized ones — `/usr/share/xsessions/lxqt.desktop` on the development machine,
    /// reproduced here so a regression in the "exact key match" logic is caught without depending
    /// on what happens to be installed on whatever machine runs `cargo test`.
    #[test]
    fn localized_name_and_comment_keys_do_not_shadow_the_unlocalized_ones() {
        let text = "[Desktop Entry]\n\
Type=Application\n\
Exec=startlxqt\n\
TryExec=lxqt-session\n\
DesktopNames=LXQt\n\
\n\
Name=LXQt Desktop\n\
Comment=Lightweight Qt Desktop\n\
Name[ar]=\u{0633}\u{0637}\u{062d}\n\
Comment[ar]=\u{0643}\u{064a}\u{0648}\u{062a}\n\
Name[de]=LXQt Arbeitsumgebung\n";
        let entry = parse_desktop_file(text, SessionKind::Wayland, &path()).unwrap();
        assert_eq!(entry.name, "LXQt Desktop");
        assert_eq!(entry.exec, vec!["startlxqt"]);
    }

    #[test]
    fn a_quoted_exec_argument_with_a_space_is_one_token() {
        let text = "[Desktop Entry]\nName=Test\nExec=\"/opt/my session/start\" --flag\n";
        let entry = parse_desktop_file(text, SessionKind::Wayland, &path()).unwrap();
        assert_eq!(entry.exec, vec!["/opt/my session/start", "--flag"]);
    }

    #[test]
    fn field_codes_are_dropped_and_percent_percent_unescapes() {
        assert_eq!(
            split_exec("gnome-session %U --name=100%%"),
            vec!["gnome-session", "--name=100%"]
        );
        assert_eq!(
            split_exec("agreety --cmd '${SHELL}'"),
            vec!["agreety", "--cmd", "'${SHELL}'"]
        );
    }

    #[test]
    fn hidden_entries_are_excluded() {
        let text = "[Desktop Entry]\nName=Secret\nExec=secret\nHidden=true\n";
        assert!(parse_desktop_file(text, SessionKind::Wayland, &path()).is_none());
    }

    #[test]
    fn no_display_entries_are_excluded() {
        let text = "[Desktop Entry]\nName=Backend\nExec=backend\nNoDisplay=true\n";
        assert!(parse_desktop_file(text, SessionKind::Wayland, &path()).is_none());
    }

    #[test]
    fn a_missing_name_or_exec_is_rejected() {
        assert!(
            parse_desktop_file("[Desktop Entry]\nExec=foo\n", SessionKind::Wayland, &path())
                .is_none()
        );
        assert!(
            parse_desktop_file("[Desktop Entry]\nName=Foo\n", SessionKind::Wayland, &path())
                .is_none()
        );
        assert!(parse_desktop_file("[Desktop Entry]\n", SessionKind::Wayland, &path()).is_none());
        assert!(parse_desktop_file("", SessionKind::Wayland, &path()).is_none());
    }

    #[test]
    fn keys_outside_the_desktop_entry_group_are_ignored() {
        // `[Desktop Action foo]` groups (extra launcher actions) must not leak their own Name/Exec
        // into the session's own.
        let text = "[Desktop Entry]\nName=Real\nExec=real-cmd\n\n[Desktop Action foo]\nName=Fake\nExec=fake-cmd\n";
        let entry = parse_desktop_file(text, SessionKind::Wayland, &path()).unwrap();
        assert_eq!(entry.name, "Real");
        assert_eq!(entry.exec, vec!["real-cmd"]);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let text = "[Desktop Entry]\n# a comment\n\nName=Real\nExec=real-cmd\n";
        let entry = parse_desktop_file(text, SessionKind::Wayland, &path()).unwrap();
        assert_eq!(entry.name, "Real");
    }

    #[test]
    fn discover_reads_desktop_files_from_a_directory_sorted_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("b-session.desktop"),
            "[Desktop Entry]\nName=B\nExec=b-cmd\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a-session.desktop"),
            "[Desktop Entry]\nName=A\nExec=a-cmd\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("not-a-session.txt"), "ignored").unwrap();
        std::fs::write(
            dir.path().join("broken.desktop"),
            "[Desktop Entry]\nName=OnlyName\n",
        )
        .unwrap();

        let entries = discover(&[(SessionKind::Wayland, dir.path().to_owned())]);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"],
            "sorted by filename, and the malformed/non-.desktop files are skipped, not fatal"
        );
    }

    #[test]
    fn discover_across_a_missing_directory_and_a_real_one_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("s.desktop"),
            "[Desktop Entry]\nName=S\nExec=s-cmd\n",
        )
        .unwrap();
        let missing = PathBuf::from("/this/path/should/not/exist/on/any/test/machine");

        let entries = discover(&[
            (SessionKind::Wayland, missing),
            (SessionKind::Wayland, dir.path().to_owned()),
        ]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "S");
    }

    #[test]
    fn default_index_prefers_legeos_when_present() {
        let entries = vec![
            SessionEntry {
                name: "COSMIC".to_owned(),
                exec: vec!["start-cosmic".to_owned()],
                kind: SessionKind::Wayland,
                source: path(),
            },
            SessionEntry {
                name: "LegeOS".to_owned(),
                exec: vec!["lege-session".to_owned()],
                kind: SessionKind::Wayland,
                source: path(),
            },
        ];
        assert_eq!(default_index(&entries), 1);
    }

    #[test]
    fn default_index_falls_back_to_the_first_entry_when_legeos_is_absent() {
        let entries = vec![SessionEntry {
            name: "COSMIC".to_owned(),
            exec: vec!["start-cosmic".to_owned()],
            kind: SessionKind::Wayland,
            source: path(),
        }];
        assert_eq!(default_index(&entries), 0);
    }

    #[test]
    fn default_index_on_an_empty_list_is_zero_not_a_panic() {
        assert_eq!(default_index(&[]), 0);
    }
}
