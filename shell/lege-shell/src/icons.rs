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
//! * **Implement the freedesktop icon theme specification.** No index.theme parsing and no theme
//!   inheritance: this walks the conventional `hicolor` size buckets and `pixmaps` directly. A
//!   name that resolves to nothing yields `None`, and callers draw a placeholder — never a panic,
//!   never a stale icon.
//!
//! Two things it *does* do, both of which it did not before the launcher needed them:
//!
//! * **SVG.** A stock hicolor theme ships mostly SVGs, so a PNG-only lookup resolved almost
//!   nothing on a real installation. Scalable icons are rasterized through `resvg` at
//!   [`SVG_RENDER_SIZE`] and preferred over raster buckets, because one vector re-rendered is
//!   sharper than a 32px PNG stretched.
//! * **Canonical ids that are not file names.** An imported application's canonical id is
//!   `desktop-import.<sanitised desktop file id>` (`lege_appd::desktop_entry::derive_app_id`), so
//!   looking for a file literally named `{app_id}.desktop` could never match one — every icon on
//!   the field was a placeholder for exactly the applications a person actually has installed.
//!   [`desktop_file_candidates`] undoes that namespacing, and a caller holding the manifest can
//!   pass the real `primary_desktop_file_id`/`aliases.desktop_file_ids` (and the `Icon=` value
//!   itself) through [`IconHints`] instead of relying on the guess.

use std::path::{Path, PathBuf};

use pixelkit_raster::Bitmap;

/// The prefix `lege_appd::desktop_entry::derive_app_id` puts on an imported application's id.
const IMPORT_PREFIX: &str = "desktop-import.";

/// The square an SVG icon is rasterized into. Matches the largest raster bucket searched, so a
/// scalable and a PNG icon reach the drawing code at the same scale.
pub const SVG_RENDER_SIZE: u32 = 128;

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

    /// Ubuntu's own theme first, then Adwaita, then `hicolor` — the freedesktop fallback of last
    /// resort. Place and device icons (`folder-documents`, `drive-harddisk`, `user-trash`) live in
    /// the named themes and almost never in `hicolor`, so searching `hicolor` alone is why the
    /// Files object had words where every other desktop has pictures.
    fn theme_dirs(&self) -> Vec<PathBuf> {
        Self::data_dirs()
            .into_iter()
            .flat_map(|dir| {
                let icons = dir.join("icons");
                THEME_NAMES.iter().map(move |theme| icons.join(theme))
            })
            .collect()
    }

    fn pixmap_dirs(&self) -> Vec<PathBuf> {
        vec![PathBuf::from("/usr/share/pixmaps")]
    }
}

/// Size buckets tried, in order. `scalable` leads because an SVG is re-rendered at the size we
/// want rather than resampled; after it, largest raster first — a tile would rather down-sample a
/// 128px icon than up-sample a 16px one into a blur.
const SIZE_BUCKETS: &[&str] = &[
    "scalable", "256x256", "128x128", "64x64", "48x48", "32x32", "24x24", "22x22", "16x16",
];

/// Icon themes searched, in order. Yaru is Ubuntu's and the one a LegeOS image installs; Adwaita
/// is what a stock GNOME stack leaves behind and is present on every dev host; `hicolor` is the
/// specified fallback every theme inherits from.
const THEME_NAMES: &[&str] = &["Yaru", "Adwaita", "hicolor"];

/// Icon-theme context directories searched inside one size bucket, in order. `apps` leads because
/// the overwhelming majority of lookups are application icons.
const ICON_CONTEXTS: &[&str] = &["apps", "places", "mimetypes", "devices", "status"];

/// File extensions tried within one bucket, in order of preference.
const ICON_EXTENSIONS: &[&str] = &["svg", "png"];

/// What the caller already knows about an application, so the lookup does not have to guess it
/// back out of the canonical id. Every field is optional; an empty [`IconHints`] reproduces the
/// name-only search.
#[derive(Clone, Copy, Debug, Default)]
pub struct IconHints<'a> {
    /// The manifest's `Icon=` value. When present, no `.desktop` file is read at all.
    pub icon: Option<&'a str>,
    /// Known `.desktop` file ids for this application (`primary_desktop_file_id` first, then
    /// `aliases.desktop_file_ids`), with or without the `.desktop` suffix.
    pub desktop_file_ids: &'a [String],
}

/// The `.desktop` file names worth trying for a canonical application id, most likely first.
///
/// A native manifest's id *is* the desktop file's base name (`org.legeos.Files` →
/// `org.legeos.Files.desktop`). An imported one is not: `derive_app_id` namespaces the whole
/// desktop file id — including its `.desktop` suffix — under `desktop-import.`, so
/// `desktop-import.firefox.desktop` is the file `firefox.desktop` and never
/// `desktop-import.firefox.desktop.desktop`, which is what the old lookup asked for.
///
/// The namespacing also replaces every character outside `[A-Za-z0-9.\-_]` with `-`, which is not
/// reversible; a desktop file whose id contained such a character is simply not found this way,
/// and that is what [`IconHints::desktop_file_ids`] is for.
pub fn desktop_file_candidates(app_id: &str) -> Vec<String> {
    if app_id.is_empty() {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut push = |name: String| {
        if !names.contains(&name) {
            names.push(name);
        }
    };
    if let Some(imported) = app_id.strip_prefix(IMPORT_PREFIX)
        && !imported.is_empty()
    {
        push(with_desktop_suffix(imported));
    }
    push(with_desktop_suffix(app_id));
    names
}

fn with_desktop_suffix(id: &str) -> String {
    if id.ends_with(".desktop") {
        id.to_owned()
    } else {
        format!("{id}.desktop")
    }
}

/// Find a `.desktop` file for `app_id` under `roots.desktop_dirs()` and parse its `Icon=` value,
/// without touching the filesystem at all if `app_id` is empty.
pub fn desktop_icon_name(app_id: &str, roots: &impl IconRoots) -> Option<String> {
    desktop_icon_name_hinted(app_id, IconHints::default(), roots)
}

/// [`desktop_icon_name`], preferring the caller's own knowledge: an `Icon=` value it already
/// holds short-circuits the search entirely, and the desktop file ids it names are tried before
/// the ones guessed from `app_id`.
pub fn desktop_icon_name_hinted(
    app_id: &str,
    hints: IconHints<'_>,
    roots: &impl IconRoots,
) -> Option<String> {
    if let Some(icon) = hints.icon.filter(|icon| !icon.is_empty()) {
        return Some(icon.to_owned());
    }
    let mut names: Vec<String> = hints
        .desktop_file_ids
        .iter()
        .filter(|id| !id.is_empty())
        .map(|id| with_desktop_suffix(id))
        .collect();
    for guess in desktop_file_candidates(app_id) {
        if !names.contains(&guess) {
            names.push(guess);
        }
    }
    if names.is_empty() {
        return None;
    }
    for dir in roots.desktop_dirs() {
        for name in &names {
            let path = dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Some(entry) = parse_desktop_entry(&text)
                && let Some(icon) = entry.icon
            {
                return Some(icon);
            }
        }
    }
    None
}

/// Resolve an icon name (or, per the desktop entry spec, an already-absolute path) to a `.png` or
/// `.svg` file. `None` when nothing matches.
pub fn resolve_icon_path(icon: &str, roots: &impl IconRoots) -> Option<PathBuf> {
    if icon.is_empty() {
        return None;
    }
    let candidate = Path::new(icon);
    if candidate.is_absolute() {
        return decodable(candidate).then(|| candidate.to_path_buf());
    }
    // An `Icon=` value is specified to be a bare name, but entries in the wild write
    // `firefox.png`; strip a known extension so the bucket search does not look for
    // `firefox.png.svg`.
    let stem = match candidate.extension() {
        Some(extension)
            if ICON_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known)) =>
        {
            candidate.file_stem()?.to_string_lossy().into_owned()
        }
        _ => icon.to_owned(),
    };
    for theme_dir in roots.theme_dirs() {
        for size in SIZE_BUCKETS {
            for context in ICON_CONTEXTS {
                for extension in ICON_EXTENSIONS {
                    let path = theme_dir
                        .join(size)
                        .join(context)
                        .join(format!("{stem}.{extension}"));
                    if path.is_file() {
                        return Some(path);
                    }
                }
            }
        }
    }
    for pixmap_dir in roots.pixmap_dirs() {
        for extension in ICON_EXTENSIONS {
            let path = pixmap_dir.join(format!("{stem}.{extension}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn decodable(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        ICON_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

/// Decode whatever `path` holds, by extension. `None` for anything unreadable or undecodable —
/// an icon is always optional, never something a surface refuses to render over.
pub fn decode_icon_file(path: &Path) -> Option<Bitmap> {
    let bytes = std::fs::read(path).ok()?;
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    {
        rasterize_svg(&bytes, SVG_RENDER_SIZE)
    } else {
        pixelkit_raster::png::decode(&bytes).ok()
    }
}

/// Rasterize an SVG into a `size`-square bitmap, letterboxed and centred so a non-square icon
/// keeps its aspect ratio rather than being stretched.
///
/// Text inside an icon is not drawn: `resvg` is linked without its font stack (see this crate's
/// `Cargo.toml`). Icons that are typography are rare and the alternative is a font database in a
/// shell that must restart quickly.
pub fn rasterize_svg(bytes: &[u8], size: u32) -> Option<Bitmap> {
    if size == 0 {
        return None;
    }
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(bytes, &options).ok()?;
    let source = tree.size();
    let (width, height) = (source.width(), source.height());
    if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (size as f32 / width).min(size as f32 / height);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let transform = resvg::tiny_skia::Transform::from_translate(
        (size as f32 - width * scale) / 2.0,
        (size as f32 - height * scale) / 2.0,
    )
    .pre_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny-skia paints premultiplied RGBA; `Bitmap` is straight-alpha `0xAARRGGBB`.
    let pixels = pixmap
        .pixels()
        .iter()
        .map(|pixel| {
            let colour = pixel.demultiply();
            (u32::from(colour.alpha()) << 24)
                | (u32::from(colour.red()) << 16)
                | (u32::from(colour.green()) << 8)
                | u32::from(colour.blue())
        })
        .collect();
    Some(Bitmap {
        width: size,
        height: size,
        pixels,
    })
}

/// The whole lookup, `AppId` string to decoded bitmap: find the `.desktop` file, read its
/// `Icon=`, resolve that to a PNG or SVG, decode it. `None` at any step yields `None` rather than
/// an error.
pub fn load_app_icon(app_id: &str, roots: &impl IconRoots) -> Option<Bitmap> {
    load_app_icon_hinted(app_id, IconHints::default(), roots)
}

/// [`load_app_icon`] for a caller that already holds the application's manifest data.
pub fn load_app_icon_hinted(
    app_id: &str,
    hints: IconHints<'_>,
    roots: &impl IconRoots,
) -> Option<Bitmap> {
    let icon_name = desktop_icon_name_hinted(app_id, hints, roots)?;
    let path = resolve_icon_path(&icon_name, roots)?;
    decode_icon_file(&path)
}

/// A named icon straight from the theme, with no `.desktop` file in between: what a folder, a
/// drive or the wastebasket is drawn with.
///
/// The name is a freedesktop icon name (`folder-documents`, `drive-harddisk`, `user-trash`), so
/// the picture is whatever the installed theme says it is rather than something this shell drew.
pub fn load_theme_icon(name: &str, roots: &impl IconRoots) -> Option<Bitmap> {
    decode_icon_file(&resolve_icon_path(name, roots)?)
}

/// The freedesktop icon name for a directory, by what the directory *is*: the user's home, one of
/// the XDG user directories, or a folder like any other.
///
/// Matching is on the path itself rather than on a label, because the label is what the person
/// renamed the row to and the icon should not follow it.
pub fn directory_icon_name(path: &Path, home: Option<&Path>) -> &'static str {
    if home.is_some_and(|home| home == path) {
        return "user-home";
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return "folder";
    };
    match name.to_ascii_lowercase().as_str() {
        "desktop" => "user-desktop",
        "documents" => "folder-documents",
        "downloads" | "download" => "folder-download",
        "music" => "folder-music",
        "pictures" => "folder-pictures",
        "videos" | "movies" => "folder-videos",
        "public" | "publicshare" => "folder-publicshare",
        "templates" => "folder-templates",
        _ => "folder",
    }
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
    fn an_absolute_svg_value_resolves_now_that_there_is_a_rasterizer_for_it() {
        let (_dir, roots) = fixture();
        let svg = roots.pixmaps.join("vector.svg");
        fs::write(&svg, b"<svg/>").unwrap();
        assert_eq!(resolve_icon_path(svg.to_str().unwrap(), &roots), Some(svg));
    }

    #[test]
    fn an_absolute_value_this_decoder_cannot_read_is_rejected_rather_than_attempted() {
        let (_dir, roots) = fixture();
        let xpm = roots.pixmaps.join("legacy.xpm");
        fs::write(&xpm, b"/* XPM */").unwrap();
        assert_eq!(resolve_icon_path(xpm.to_str().unwrap(), &roots), None);
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

    #[test]
    fn an_imported_app_ids_desktop_file_is_found_under_its_real_name() {
        // The regression this whole module existed to have and did not: `desktop-import.` ids
        // are what the registry hands the shell for every application it did not ship itself.
        let (_dir, roots) = fixture();
        fs::write(
            roots.desktop.join("firefox.desktop"),
            "[Desktop Entry]\nName=Firefox\nIcon=firefox\n",
        )
        .unwrap();
        assert_eq!(
            desktop_icon_name("desktop-import.firefox.desktop", &roots).as_deref(),
            Some("firefox"),
        );
    }

    #[test]
    fn desktop_file_candidates_cover_both_shapes_of_canonical_id_and_nothing_else() {
        assert_eq!(
            desktop_file_candidates("desktop-import.firefox.desktop"),
            ["firefox.desktop", "desktop-import.firefox.desktop"],
        );
        assert_eq!(
            desktop_file_candidates("org.legeos.Files"),
            ["org.legeos.Files.desktop"],
        );
        assert!(desktop_file_candidates("").is_empty());
    }

    #[test]
    fn a_known_icon_name_short_circuits_the_desktop_file_search_entirely() {
        let (_dir, roots) = fixture();
        // No `.desktop` file exists, and none is needed: the manifest already said `Icon=`.
        let hints = IconHints {
            icon: Some("editor-icon"),
            ..IconHints::default()
        };
        assert_eq!(
            desktop_icon_name_hinted("desktop-import.weird name.desktop", hints, &roots).as_deref(),
            Some("editor-icon"),
        );
    }

    #[test]
    fn a_known_desktop_file_id_beats_a_guess_that_the_sanitiser_made_unreversible() {
        let (_dir, roots) = fixture();
        // `derive_app_id` turned the space into `-`, so no guess can find this file.
        fs::write(
            roots.desktop.join("odd name.desktop"),
            "[Desktop Entry]\nName=Odd\nIcon=odd-icon\n",
        )
        .unwrap();
        let ids = ["odd name.desktop".to_owned()];
        let hints = IconHints {
            desktop_file_ids: &ids,
            ..IconHints::default()
        };
        assert_eq!(
            desktop_icon_name_hinted("desktop-import.odd-name.desktop", hints, &roots).as_deref(),
            Some("odd-icon"),
        );
    }

    #[test]
    fn a_scalable_icon_is_preferred_over_a_raster_bucket_and_rasterizes() {
        let (_dir, roots) = fixture();
        fs::create_dir_all(roots.theme.join("scalable/apps")).unwrap();
        fs::create_dir_all(roots.theme.join("128x128/apps")).unwrap();
        fs::write(roots.theme.join("128x128/apps/editor.png"), b"not a png").unwrap();
        fs::write(
            roots.theme.join("scalable/apps/editor.svg"),
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        let path = resolve_icon_path("editor", &roots).unwrap();
        assert!(path.ends_with("scalable/apps/editor.svg"), "{path:?}");

        let bitmap = decode_icon_file(&path).unwrap();
        assert_eq!(
            (bitmap.width, bitmap.height),
            (SVG_RENDER_SIZE, SVG_RENDER_SIZE)
        );
        // Opaque red, straight alpha, in the middle of a square icon.
        let middle = bitmap.pixel(SVG_RENDER_SIZE / 2, SVG_RENDER_SIZE / 2);
        assert_eq!(middle, 0xffff_0000, "got {middle:#010x}");
    }

    #[test]
    fn a_non_square_svg_keeps_its_aspect_ratio_instead_of_stretching() {
        let bitmap = rasterize_svg(
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="16"><rect width="32" height="16" fill="#0000ff"/></svg>"##,
            32,
        )
        .unwrap();
        assert_eq!((bitmap.width, bitmap.height), (32, 32));
        // The drawing fills the width and is letterboxed top and bottom.
        assert_eq!(bitmap.pixel(16, 16), 0xff00_00ff);
        assert_eq!(bitmap.pixel(16, 1) >> 24, 0, "the top band must stay clear");
    }

    #[test]
    fn a_file_that_is_not_svg_at_all_yields_none_rather_than_a_panic() {
        assert_eq!(rasterize_svg(b"certainly not markup", 32), None);
    }

    #[test]
    fn an_icon_name_written_with_its_extension_still_finds_the_bucket_file() {
        let (_dir, roots) = fixture();
        fs::create_dir_all(roots.theme.join("48x48/apps")).unwrap();
        fs::write(roots.theme.join("48x48/apps/editor.png"), b"x").unwrap();
        let path = resolve_icon_path("editor.png", &roots).unwrap();
        assert!(path.ends_with("48x48/apps/editor.png"), "{path:?}");
    }
}
