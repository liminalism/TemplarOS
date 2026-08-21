//! Best-effort icon lookup: an `AppId` to a decoded [`Bitmap`], via a `.desktop` file's `Icon=`
//! key and a simplified freedesktop icon-theme search.
//!
//! Two things this deliberately does not do:
//!
//! * **Reuse `lege_appd::desktop_entry`.** That parser is solid and already tested, but
//!   `lege-appd` is a service-hosting crate — it pulls in `rusqlite` (bundled), `zbus_systemd`
//!   and `lege-state` for its registry/store/systemd responsibilities, none of which a restartable
//!   decorative shell has any business linking merely to read one `Icon=` line. `format.rs` in
//!   this same crate already made this call once (duplicating `cli/lege`'s byte/timestamp
//!   formatting rather than sharing a crate for four lines of arithmetic); the same reasoning
//!   applies here, for a still-smaller parser.
//! * **Implement the freedesktop icon theme specification.** No index.theme parsing, no theme
//!   inheritance, no scalable/SVG icons (`pixelkit-raster::png` only decodes PNG — see the
//!   milestone report for what that costs in practice against a stock hicolor theme, which ships
//!   mostly SVGs). This walks the conventional `hicolor` size buckets and `pixmaps` directly. A
//!   name that resolves to nothing yields `None`, and callers draw a placeholder — never a panic,
//!   never a stale icon.

use std::path::{Path, PathBuf};

use pixelkit_raster::Bitmap;

/// The `.desktop` `[Desktop Entry]` fields this module reads. A trimmed-down mirror of
/// `lege_appd::desktop_entry::DesktopEntry` — see the module doc for why this does not depend on
/// that crate directly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopIcon {
    pub name: String,
    pub icon: Option<String>,
}

/// Parse the `[Desktop Entry]` group's `Name=`/`Icon=` keys. `None` if the file has no such group.
///
/// Deliberately reads only these two keys — this module has no use for `Exec=`, MIME types or any
/// of the rest of the format, and every key this does not recognise is simply skipped rather than
/// causing a parse error, the same tolerance `lege_appd::desktop_entry::parse` applies.
pub fn parse_desktop_entry(text: &str) -> Option<DesktopIcon> {
    let mut in_main_group = false;
    let mut saw_main_group = false;
    let mut entry = DesktopIcon::default();
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
            continue; // a localized variant; the unlocalized key wins.
        }
        match key {
            "Name" => value.clone_into(&mut entry.name),
            "Icon" => entry.icon = Some(value.to_owned()),
            _ => {}
        }
    }
    saw_main_group.then_some(entry)
}

/// Where to look for `.desktop` files and icon themes — a trait so resolution is testable against
/// a fixture directory tree rather than the real filesystem.
pub trait IconRoots {
    /// Directories searched, in order, for `{app_id}.desktop`.
    fn desktop_dirs(&self) -> Vec<PathBuf>;
    /// Icon theme directories (each containing `<size>/apps/<name>.png` and similar), searched in
    /// order.
    fn theme_dirs(&self) -> Vec<PathBuf>;
    /// Flat pixmap directories (`<name>.png` directly inside), searched after theme directories.
    fn pixmap_dirs(&self) -> Vec<PathBuf>;
}

/// The real filesystem, following `$XDG_DATA_HOME`/`$XDG_DATA_DIRS` (falling back to the usual
/// defaults exactly as `standard_paths.rs` falls back for user directories) plus the conventional
/// `/usr/share/pixmaps`.
#[derive(Debug, Clone, Default)]
pub struct SystemIconRoots;

impl SystemIconRoots {
    fn data_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = std::env::var_os("XDG_DATA_HOME") {
            dirs.push(PathBuf::from(home));
        } else if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share"));
        }
        let extra = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
        dirs.extend(
            extra
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
        dirs
    }
}

impl IconRoots for SystemIconRoots {
    fn desktop_dirs(&self) -> Vec<PathBuf> {
        Self::data_dirs()
            .into_iter()
            .map(|dir| dir.join("applications"))
            .collect()
    }

    fn theme_dirs(&self) -> Vec<PathBuf> {
        Self::data_dirs()
            .into_iter()
            .map(|dir| dir.join("icons/hicolor"))
            .collect()
    }

    fn pixmap_dirs(&self) -> Vec<PathBuf> {
        vec![PathBuf::from("/usr/share/pixmaps")]
    }
}

/// Size buckets tried, largest first — a dock or a drawer row would rather down-sample a 128px
/// icon than up-sample a 16px one into a blur.
const SIZE_BUCKETS: &[&str] = &["128x128", "64x64", "48x48", "32x32", "scalable"];

/// Find `{app_id}.desktop` under `roots.desktop_dirs()` and parse its `Icon=` value, without
/// touching the filesystem at all if `app_id` is empty.
pub fn desktop_icon_name(app_id: &str, roots: &impl IconRoots) -> Option<String> {
    if app_id.is_empty() {
        return None;
    }
    for dir in roots.desktop_dirs() {
        let path = dir.join(format!("{app_id}.desktop"));
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some(entry) = parse_desktop_entry(&text)
        {
            return entry.icon;
        }
    }
    None
}

/// Resolve an icon name (or, per the desktop entry spec, an already-absolute path) to a `.png`
/// file. `None` when nothing matches — SVG-only themes are the common case this returns `None`
/// for; see the module doc.
pub fn resolve_icon_path(icon: &str, roots: &impl IconRoots) -> Option<PathBuf> {
    let candidate = Path::new(icon);
    if candidate.is_absolute() {
        return candidate
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
            .then(|| candidate.to_path_buf());
    }
    for theme_dir in roots.theme_dirs() {
        for size in SIZE_BUCKETS {
            let path = theme_dir
                .join(size)
                .join("apps")
                .join(format!("{icon}.png"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    for pixmap_dir in roots.pixmap_dirs() {
        let path = pixmap_dir.join(format!("{icon}.png"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// The whole lookup, `AppId` string to decoded bitmap: find the `.desktop` file, read its
/// `Icon=`, resolve that to a PNG, decode it. `None` at any step (no `.desktop` file, no `Icon=`
/// key, no PNG matching that name, or a PNG this decoder cannot read) yields `None` rather than an
/// error — an icon is always optional, never something a drawer refuses to render over.
pub fn load_app_icon(app_id: &str, roots: &impl IconRoots) -> Option<Bitmap> {
    let icon_name = desktop_icon_name(app_id, roots)?;
    let path = resolve_icon_path(&icon_name, roots)?;
    let bytes = std::fs::read(path).ok()?;
    pixelkit_raster::png::decode(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    struct FixtureRoots {
        desktop: PathBuf,
        theme: PathBuf,
        pixmaps: PathBuf,
    }

    impl IconRoots for FixtureRoots {
        fn desktop_dirs(&self) -> Vec<PathBuf> {
            vec![self.desktop.clone()]
        }
        fn theme_dirs(&self) -> Vec<PathBuf> {
            vec![self.theme.clone()]
        }
        fn pixmap_dirs(&self) -> Vec<PathBuf> {
            vec![self.pixmaps.clone()]
        }
    }

    fn fixture() -> (TempDir, FixtureRoots) {
        let dir = TempDir::new().unwrap();
        let roots = FixtureRoots {
            desktop: dir.path().join("applications"),
            theme: dir.path().join("hicolor"),
            pixmaps: dir.path().join("pixmaps"),
        };
        fs::create_dir_all(&roots.desktop).unwrap();
        fs::create_dir_all(&roots.theme).unwrap();
        fs::create_dir_all(&roots.pixmaps).unwrap();
        (dir, roots)
    }

    #[test]
    fn a_desktop_entry_group_parses_name_and_icon() {
        let text = "[Desktop Entry]\nType=Application\nName=Editor\nIcon=org.legeos.Editor\n";
        let entry = parse_desktop_entry(text).unwrap();
        assert_eq!(entry.name, "Editor");
        assert_eq!(entry.icon.as_deref(), Some("org.legeos.Editor"));
    }

    #[test]
    fn a_localized_name_does_not_shadow_the_unlocalized_one() {
        let text = "[Desktop Entry]\nName=Editor\nName[fr]=Éditeur\nIcon=editor\n";
        let entry = parse_desktop_entry(text).unwrap();
        assert_eq!(
            entry.name, "Editor",
            "the localized key must be skipped, not overwrite this"
        );
    }

    #[test]
    fn a_file_with_no_main_group_is_none() {
        assert_eq!(parse_desktop_entry("[X-Something]\nFoo=bar\n"), None);
    }

    #[test]
    fn desktop_icon_name_finds_the_file_named_after_the_app_id() {
        let (_dir, roots) = fixture();
        fs::write(
            roots.desktop.join("org.legeos.Editor.desktop"),
            "[Desktop Entry]\nName=Editor\nIcon=editor-icon\n",
        )
        .unwrap();
        assert_eq!(
            desktop_icon_name("org.legeos.Editor", &roots).as_deref(),
            Some("editor-icon")
        );
    }

    #[test]
    fn an_empty_app_id_never_touches_the_filesystem() {
        let (_dir, roots) = fixture();
        // No file at all under `roots.desktop` — if this looked anyway it would find nothing
        // either way, so what this really asserts is the early return before any I/O happens.
        assert_eq!(desktop_icon_name("", &roots), None);
    }

    #[test]
    fn a_missing_desktop_file_resolves_to_nothing_rather_than_an_error() {
        let (_dir, roots) = fixture();
        assert_eq!(desktop_icon_name("org.legeos.Nowhere", &roots), None);
    }

    #[test]
    fn resolve_icon_path_prefers_the_largest_size_bucket_present() {
        let (_dir, roots) = fixture();
        fs::create_dir_all(roots.theme.join("48x48/apps")).unwrap();
        fs::create_dir_all(roots.theme.join("128x128/apps")).unwrap();
        fs::write(roots.theme.join("48x48/apps/editor.png"), b"small").unwrap();
        fs::write(roots.theme.join("128x128/apps/editor.png"), b"large").unwrap();
        let path = resolve_icon_path("editor", &roots).unwrap();
        assert!(path.ends_with("128x128/apps/editor.png"), "{path:?}");
    }

    #[test]
    fn resolve_icon_path_falls_back_to_pixmaps() {
        let (_dir, roots) = fixture();
        fs::write(roots.pixmaps.join("editor.png"), b"pixmap").unwrap();
        let path = resolve_icon_path("editor", &roots).unwrap();
        assert!(path.ends_with("pixmaps/editor.png"), "{path:?}");
    }

    #[test]
    fn an_absolute_icon_value_is_used_directly_when_it_is_a_png() {
        let (_dir, roots) = fixture();
        let absolute = roots.pixmaps.join("direct.png");
        fs::write(&absolute, b"direct").unwrap();
        assert_eq!(
            resolve_icon_path(absolute.to_str().unwrap(), &roots),
            Some(absolute)
        );
    }

    #[test]
    fn an_absolute_non_png_value_is_rejected_rather_than_handed_to_the_png_decoder() {
        let (_dir, roots) = fixture();
        let svg = roots.pixmaps.join("vector.svg");
        fs::write(&svg, b"<svg/>").unwrap();
        assert_eq!(resolve_icon_path(svg.to_str().unwrap(), &roots), None);
    }

    #[test]
    fn an_icon_name_matching_nothing_resolves_to_none() {
        let (_dir, roots) = fixture();
        assert_eq!(resolve_icon_path("does-not-exist", &roots), None);
    }

    #[test]
    fn load_app_icon_decodes_a_real_png_end_to_end() {
        let (_dir, roots) = fixture();
        fs::write(
            roots.desktop.join("org.legeos.Editor.desktop"),
            "[Desktop Entry]\nName=Editor\nIcon=editor-icon\n",
        )
        .unwrap();
        fs::create_dir_all(roots.theme.join("48x48/apps")).unwrap();
        let buffer = pixelkit_raster::WindowBuffer::new(4, 4);
        let png = pixelkit_raster::png::encode(&buffer);
        fs::write(roots.theme.join("48x48/apps/editor-icon.png"), &png).unwrap();

        let loaded = load_app_icon("org.legeos.Editor", &roots).unwrap();
        assert_eq!((loaded.width, loaded.height), (4, 4));
    }

    #[test]
    fn an_app_with_no_desktop_file_at_all_loads_no_icon() {
        let (_dir, roots) = fixture();
        assert_eq!(load_app_icon("org.legeos.Nowhere", &roots), None);
    }
}
