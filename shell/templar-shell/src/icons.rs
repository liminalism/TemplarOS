//! Best-effort icon lookup: an `AppId` to a decoded [`Bitmap`], via a `.desktop` file's `Icon=`
//! key and a simplified freedesktop icon-theme search.
//!
//! Two things this deliberately does not do:
//!
//! * **Reuse `templar_appd::desktop_entry`.** That parser is solid and already tested, but
//!   `templar-appd` is a service-hosting crate — it pulls in `rusqlite` (bundled), `zbus_systemd`
//!   and `templar-state` for its registry/store/systemd responsibilities, none of which a restartable
//!   decorative shell has any business linking merely to read one `Icon=` line. `format.rs` in
//!   this same crate already made this call once (duplicating `cli/templar`'s byte/timestamp
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
//!   `desktop-import.<sanitised desktop file id>` (`templar_appd::desktop_entry::derive_app_id`), so
//!   looking for a file literally named `{app_id}.desktop` could never match one — every icon on
//!   the field was a placeholder for exactly the applications a person actually has installed.
//!   [`desktop_file_candidates`] undoes that namespacing, and a caller holding the manifest can
//!   pass the real `primary_desktop_file_id`/`aliases.desktop_file_ids` (and the `Icon=` value
//!   itself) through [`IconHints`] instead of relying on the guess.

use std::path::{Path, PathBuf};

use pixelkit_raster::Bitmap;

/// The prefix `templar_appd::desktop_entry::derive_app_id` puts on an imported application's id.
const IMPORT_PREFIX: &str = "desktop-import.";

/// The square an SVG icon is rasterized into. Matches the largest raster bucket searched, so a
/// scalable and a PNG icon reach the drawing code at the same scale.
pub const SVG_RENDER_SIZE: u32 = 128;

/// The `.desktop` `[Desktop Entry]` fields this module reads. A trimmed-down mirror of
/// `templar_appd::desktop_entry::DesktopEntry` — see the module doc for why this does not depend on
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
/// causing a parse error, the same tolerance `templar_appd::desktop_entry::parse` applies.
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

    /// Every directory an icon file could sit in, best first: each theme's own subdirectories,
    /// discovered rather than assumed. See [`ranked_icon_dirs`] for why they cannot be assumed.
    fn icon_dirs(&self) -> Vec<PathBuf> {
        self.theme_dirs()
            .iter()
            .flat_map(|theme| ranked_icon_dirs(theme))
            .collect()
    }
}

/// How deep inside a theme an icon directory is allowed to be. Two is the common shape
/// (`Papirus/48x48/apps`, and `breeze/apps/48` with the two the other way round); `oxygen` puts a
/// `base` in front of it and needs three.
const MAX_THEME_DEPTH: usize = 3;

/// The directories inside one icon theme that hold icon files, largest and most scalable first.
///
/// Discovered by reading the theme, not composed from a table of sizes and contexts: the four
/// themes on this developer's machine alone use `48x48/apps`, `apps/48`, `base/128x128/apps` and
/// `symbolic/apps`, and a fixed `<size>/<context>` product finds icons in the first of those and
/// in none of the others. That product is why 82 of 206 installed applications — including
/// TemplarOS's own Files (`system-file-manager`) and Settings (`preferences-system`) — resolved to
/// nothing and were drawn as their first letter.
///
/// Ordering is by what the drawing code wants: a vector first, then the raster bucket nearest
/// [`SVG_RENDER_SIZE`] (a down-sampled 256 beats an up-sampled 16), then by context so an
/// application icon is preferred to a mimetype of the same name. Ties keep `read_dir` order,
/// which is arbitrary but stable enough for a cache built once.
fn ranked_icon_dirs(theme: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_dirs(theme, MAX_THEME_DEPTH, &mut found);
    found.sort_by_key(|dir| dir_rank(theme, dir));
    found
}

fn collect_dirs(dir: &Path, depth: usize, into: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_dirs(&path, depth - 1, into);
            into.push(path);
        }
    }
}

/// `(size distance, context preference)` for one directory inside `theme`, smallest first.
fn dir_rank(theme: &Path, dir: &Path) -> (i64, usize) {
    let mut size = None;
    let mut context = ICON_CONTEXTS.len();
    for segment in dir
        .strip_prefix(theme)
        .unwrap_or(dir)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
    {
        if segment == "scalable" || segment == "symbolic" {
            size = Some(-1);
        } else if let Some(pixels) = bucket_size(segment) {
            size = size.or(Some(i64::from(pixels.abs_diff(SVG_RENDER_SIZE))));
        }
        if let Some(position) = ICON_CONTEXTS.iter().position(|known| *known == segment) {
            context = context.min(position);
        }
    }
    // A directory that names no size at all sorts after every one that does: `hicolor/apps` is
    // real, and so is a theme's stray `cursors`.
    (size.unwrap_or(i64::MAX), context)
}

/// The pixel size a bucket name states — `48x48`, `48x48@2x` and a bare `48` all say 48. `None`
/// for a name that is not a size at all.
fn bucket_size(segment: &str) -> Option<u32> {
    let head = segment.split(['x', '@']).next()?;
    head.parse().ok().filter(|pixels| *pixels > 0)
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

    /// [`THEME_NAMES`] first, in that order, then every other theme installed, alphabetically.
    ///
    /// The preferred list is a preference, not the search: a TemplarOS image installs Yaru, a GNOME
    /// host leaves Adwaita, and this developer's machine has neither's application icons but does
    /// have Papirus, Pop, breeze and oxygen. Naming three themes and stopping there is how stock
    /// freedesktop names — `system-file-manager`, `preferences-system`, `utilities-terminal` —
    /// resolved to nothing on a machine that had every one of them.
    fn theme_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for data in Self::data_dirs() {
            let icons = data.join("icons");
            for name in THEME_NAMES {
                dirs.push(icons.join(name));
            }
            let Ok(entries) = std::fs::read_dir(&icons) else {
                continue;
            };
            let mut rest: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir() && !dirs.contains(path))
                .collect();
            rest.sort();
            dirs.extend(rest);
        }
        dirs
    }

    /// Walked once per process and kept. The walk is ~2000 directories on a machine with a dozen
    /// themes installed; every application on the field asks for an icon, and re-walking for each
    /// of them is the difference between a few milliseconds and a few seconds of the blocking
    /// pool. A theme installed after the shell started is picked up by restarting it, which is
    /// what the shell is built to survive.
    fn icon_dirs(&self) -> Vec<PathBuf> {
        static DIRS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
        DIRS.get_or_init(|| {
            self.theme_dirs()
                .iter()
                .flat_map(|theme| ranked_icon_dirs(theme))
                .collect()
        })
        .clone()
    }

    fn pixmap_dirs(&self) -> Vec<PathBuf> {
        vec![PathBuf::from("/usr/share/pixmaps")]
    }
}

/// Icon themes preferred, in order. Yaru is Ubuntu's and the one a TemplarOS image installs; Papirus
/// is the one with a picture for every stock freedesktop name; Adwaita is what a GNOME stack
/// leaves behind; `hicolor` is both the specified fallback every theme inherits from and where an
/// application installs its *own* icon, so it comes before any theme not named here.
const THEME_NAMES: &[&str] = &["Yaru", "Papirus", "Adwaita", "Humanity", "hicolor"];

/// Icon-theme context directories, in preference order — the tie-break in [`dir_rank`], not a
/// filter: a directory naming none of these is still searched, last. `apps` leads because the
/// overwhelming majority of lookups are application icons, and `categories` is here because that
/// is where a settings icon (`preferences-system`) lives in half the themes that ship one.
const ICON_CONTEXTS: &[&str] = &[
    "apps",
    "categories",
    "places",
    "mimetypes",
    "devices",
    "status",
    "actions",
    "emblems",
];

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
/// A native manifest's id *is* the desktop file's base name (`org.templaros.Files` →
/// `org.templaros.Files.desktop`). An imported one is not: `derive_app_id` namespaces the whole
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
    let dirs = roots.icon_dirs();
    // The plain name everywhere first, and only then the monochrome variant everywhere: a
    // symbolic glyph from the best theme is still worse than the real icon from the worst.
    for name in [stem.as_str(), &format!("{stem}-symbolic")] {
        for dir in &dirs {
            for extension in ICON_EXTENSIONS {
                let path = dir.join(format!("{name}.{extension}"));
                if path.is_file() {
                    return Some(path);
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
        let text = "[Desktop Entry]\nType=Application\nName=Editor\nIcon=org.templaros.Editor\n";
        let entry = parse_desktop_entry(text).unwrap();
        assert_eq!(entry.name, "Editor");
        assert_eq!(entry.icon.as_deref(), Some("org.templaros.Editor"));
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
            roots.desktop.join("org.templaros.Editor.desktop"),
            "[Desktop Entry]\nName=Editor\nIcon=editor-icon\n",
        )
        .unwrap();
        assert_eq!(
            desktop_icon_name("org.templaros.Editor", &roots).as_deref(),
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
        assert_eq!(desktop_icon_name("org.templaros.Nowhere", &roots), None);
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

    /// The regression the fixed `<size>/<context>` product caused: `breeze` writes
    /// `apps/48/`, `oxygen` writes `base/128x128/apps/` and `Adwaita` now ships application
    /// icons only under `symbolic/apps/`. None of those is `<size>/<context>`, so on a machine
    /// carrying all of them the stock names resolved to nothing and every tile was a letter.
    #[test]
    fn every_theme_layout_in_the_wild_is_searched_not_just_size_over_context() {
        let (_dir, roots) = fixture();
        for (dir, file) in [
            ("apps/48", "breeze-shaped.png"),
            ("base/128x128/apps", "oxygen-shaped.png"),
            ("scalable/categories", "preferences-system.svg"),
        ] {
            fs::create_dir_all(roots.theme.join(dir)).unwrap();
            fs::write(roots.theme.join(dir).join(file), b"icon").unwrap();
        }
        for name in ["breeze-shaped", "oxygen-shaped", "preferences-system"] {
            assert!(
                resolve_icon_path(name, &roots).is_some(),
                "{name} was not found"
            );
        }
    }

    /// A monochrome glyph is a last resort, not a preference: the real icon wins wherever it is,
    /// and `-symbolic` is tried only once every theme has been asked for the plain name.
    #[test]
    fn a_symbolic_glyph_answers_only_when_no_theme_has_the_plain_name() {
        let (_dir, roots) = fixture();
        fs::create_dir_all(roots.theme.join("symbolic/apps")).unwrap();
        fs::create_dir_all(roots.theme.join("128x128/apps")).unwrap();
        fs::write(roots.theme.join("symbolic/apps/mapped-symbolic.svg"), b"g").unwrap();
        assert!(
            resolve_icon_path("mapped", &roots)
                .is_some_and(|path| path.ends_with("mapped-symbolic.svg"))
        );

        fs::write(roots.theme.join("symbolic/apps/both-symbolic.svg"), b"g").unwrap();
        fs::write(roots.theme.join("128x128/apps/both.png"), b"real").unwrap();
        let path = resolve_icon_path("both", &roots).unwrap();
        assert!(path.ends_with("128x128/apps/both.png"), "{path:?}");
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
            roots.desktop.join("org.templaros.Editor.desktop"),
            "[Desktop Entry]\nName=Editor\nIcon=editor-icon\n",
        )
        .unwrap();
        fs::create_dir_all(roots.theme.join("48x48/apps")).unwrap();
        let buffer = pixelkit_raster::WindowBuffer::new(4, 4);
        let png = pixelkit_raster::png::encode(&buffer);
        fs::write(roots.theme.join("48x48/apps/editor-icon.png"), &png).unwrap();

        let loaded = load_app_icon("org.templaros.Editor", &roots).unwrap();
        assert_eq!((loaded.width, loaded.height), (4, 4));
    }

    #[test]
    fn an_app_with_no_desktop_file_at_all_loads_no_icon() {
        let (_dir, roots) = fixture();
        assert_eq!(load_app_icon("org.templaros.Nowhere", &roots), None);
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
            desktop_file_candidates("org.templaros.Files"),
            ["org.templaros.Files.desktop"],
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
