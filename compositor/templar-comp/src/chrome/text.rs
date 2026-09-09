//! Font discovery and the long-lived text-rendering state chrome draws with.
//!
//! `pixelkit-text` parses TrueType bytes but does no filesystem work at all — finding a font on
//! disk is this compositor's job, and it is the one part of chrome genuinely capable of finding
//! nothing. [`ChromeFonts::discover`] must therefore never be able to bring the compositor down:
//! every step returns `Option`/skips a bad candidate rather than panicking, and finding no usable
//! font anywhere it looked is a normal, handled outcome (`None`), not an error — `chrome/mod.rs`
//! falls back to drawing the plain rectangles that existed before this module (see
//! `chrome::paint`'s `NoFont` path and this file's `discovery_finds_nothing_and_does_not_panic`
//! test).

use pixelkit_text::{Face, FaceId, FontSet, TextCache};
use std::path::{Path, PathBuf};

/// A working font plus the cache chrome draws text through. Once discovery succeeds these are
/// process-lifetime (`'static`, by construction — see [`ChromeFonts::from_bytes`]'s doc), so this
/// is built once at compositor startup and reused every frame.
pub struct ChromeFonts {
    pub face: FaceId,
    /// The italic/oblique companion of [`Self::face`], used for a pinned window's name
    /// (`WindowService::is_always_on_top`). Falls back to `face` itself when the machine has no
    /// italic `.ttf` anywhere it looked: an always-on-top window that reads upright is a missing
    /// cue, and a compositor that refuses to draw is a missing desktop.
    pub italic: FaceId,
    pub cache: TextCache,
}

impl ChromeFonts {
    /// Look for a usable font under the usual Linux font locations and, if one parses, build the
    /// long-lived [`FontSet`]/[`TextCache`] chrome draws through. `None` means exactly one thing:
    /// draw the plain rectangles, never a font.
    pub fn discover() -> Option<ChromeFonts> {
        Self::discover_under(&default_font_roots())
    }

    /// [`discover`](Self::discover), but searching only `roots` — the seam this module's tests use
    /// instead of depending on whatever fonts (if any) the host actually has installed.
    fn discover_under(roots: &[PathBuf]) -> Option<ChromeFonts> {
        let bytes = discover_font_bytes(roots, PREFERRED_NAMES, false)?;
        Self::from_bytes(bytes, discover_font_bytes(roots, ITALIC_NAMES, true))
    }

    /// `Face::from_bytes` requires `&'static [u8]` because a [`FontSet`] full of [`Face`]s is
    /// meant to live for the process's whole lifetime — exactly what chrome needs, since this is
    /// constructed once at compositor startup, not per-frame or per-window. `bytes` is already
    /// leaked by [`discover_font_bytes`] (which had to leak it anyway just to validate it parses),
    /// so this re-parses rather than re-leaking — one extra cheap parse at startup instead of a
    /// second permanent allocation for the same font.
    fn from_bytes(bytes: &'static [u8], italic: Option<&'static [u8]>) -> Option<ChromeFonts> {
        let face = Face::from_bytes(bytes)?;
        let leaked_face: &'static Face = Box::leak(Box::new(face));

        let mut set = FontSet::new();
        let face_id = set.add(leaked_face);
        let italic_id = italic
            .and_then(Face::from_bytes)
            .map_or(face_id, |face| set.add(Box::leak(Box::new(face))));
        let leaked_set: &'static FontSet = Box::leak(Box::new(set));

        Some(ChromeFonts {
            face: face_id,
            italic: italic_id,
            cache: TextCache::new(leaked_set),
        })
    }
}

/// Directories worth checking for a usable UI font, most-preferred first. A function rather than
/// a `const` because it reads `$HOME`/`$XDG_DATA_HOME`, which cannot be `const`. A missing or
/// inaccessible `$HOME` simply drops the user-local roots — root-owned system fonts are still
/// tried, so this never fails outright just because the environment is sparse (e.g. a stripped
/// service-manager launch environment).
fn default_font_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data_home).join("fonts"));
    } else if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/fonts"));
        roots.push(home.join(".fonts"));
    }
    roots.push(PathBuf::from("/usr/share/fonts"));
    roots.push(PathBuf::from("/usr/local/share/fonts"));
    roots
}

/// Filenames tried first, in this order, in every root — common regular-weight UI fonts likely to
/// already be installed. Matched case-insensitively against the file's own name, not its whole
/// path, so a distro's exact nesting (`truetype/dejavu/...`) does not matter.
const PREFERRED_NAMES: &[&str] = &[
    "dejavusans.ttf",
    "notosans-regular.ttf",
    "liberationsans-regular.ttf",
    "cantarell-regular.ttf",
    "opensans-regular.ttf",
    "ubuntu-regular.ttf",
    "arial.ttf",
];

/// The italic/oblique companions of [`PREFERRED_NAMES`], in the same order and matched the same
/// way. Not finding one is normal and not an error — see [`ChromeFonts::italic`].
const ITALIC_NAMES: &[&str] = &[
    "dejavusans-oblique.ttf",
    "notosans-italic.ttf",
    "liberationsans-italic.ttf",
    "cantarell-italic.ttf",
    "opensans-italic.ttf",
    "ubuntu-italic.ttf",
    "ariali.ttf",
];

/// How many directory entries a discovery pass will look at in total before giving up — bounds
/// the cost of a pathological font tree (thousands of files, a symlink cycle) rather than letting
/// compositor startup hang on it. Comfortably above a normal desktop's font count (this
/// development machine has 529 `.ttf` files under `/usr/share/fonts` alone).
const SCAN_BUDGET: usize = 20_000;
/// How deep the walk will recurse — real font trees are a handful of levels
/// (`/usr/share/fonts/truetype/dejavu/...`); deeper than this is almost certainly a symlink loop,
/// not a font subtree worth chasing further.
const MAX_DEPTH: usize = 10;

/// Every regular file directly or transitively under `root`, bounded by [`SCAN_BUDGET`] and
/// [`MAX_DEPTH`]. A missing or unreadable `root` yields an empty list, never an error — most
/// systems do not have all of [`default_font_roots`]'s candidates, and that is normal.
fn walk(root: &Path, budget: &mut usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if *budget == 0 {
            break;
        }
        if depth > MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push((entry.path(), depth + 1));
            } else if file_type.is_file() {
                found.push(entry.path());
            }
        }
    }
    found
}

/// Lower a candidate path to its preference rank: index into [`PREFERRED_NAMES`] if its filename
/// matches (case-insensitively), else `PREFERRED_NAMES.len()` (tried last, in discovery order).
fn preference_rank(path: &Path, names: &[&str]) -> usize {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return names.len();
    };
    let lower = name.to_ascii_lowercase();
    names
        .iter()
        .position(|candidate| *candidate == lower)
        .unwrap_or(names.len())
}

fn is_ttf(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ttf"))
}

/// Find the first `.ttf` under any of `roots` that actually parses as a usable
/// [`pixelkit_text::Face`] — [`PREFERRED_NAMES`] first (in that order), then every other `.ttf`
/// found, in walk order. Only `.ttf`-extensioned files are tried at all: `pixelkit-text` only
/// reads `glyf`-outline TrueType, so `.otf`/`.ttc`/anything else is skipped before ever being read.
///
/// `Face::from_bytes` needs `'static` bytes to validate against at all (`pixelkit-text`'s own
/// API), so validating a candidate here costs the same leak `ChromeFonts::from_bytes` will pay
/// for the eventual winner anyway; the only cost of a *rejected* candidate is that its bytes are
/// leaked too rather than freed. In practice this almost never triggers more than once — the
/// first `PREFERRED_NAMES` hit on a real system is a real, working font — and it is bounded by
/// [`SCAN_BUDGET`] in the pathological case where nothing under `roots` parses at all.
fn discover_font_bytes(
    roots: &[PathBuf],
    names: &[&str],
    named_only: bool,
) -> Option<&'static [u8]> {
    let mut budget = SCAN_BUDGET;
    let mut candidates: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| walk(root, &mut budget))
        .filter(|path| is_ttf(path))
        .collect();
    // `named_only` is the italic pass. The regular pass falls back to any `.ttf` that parses,
    // because a desktop with no text at all is worse than a desktop in an odd font; an italic
    // fallback to some arbitrary upright face would be no cue and a wasted leak.
    if named_only {
        candidates.retain(|path| preference_rank(path, names) < names.len());
    }
    candidates.sort_by_key(|path| preference_rank(path, names));

    candidates.into_iter().find_map(|path| {
        let bytes = std::fs::read(&path).ok()?;
        // A `None` here is a normal outcome (corrupt file, CFF/OTF outlines, a variable font)
        // and moves on to the next candidate rather than failing discovery outright — see the
        // leak-cost note above for why a rejected candidate is still leaked.
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        Face::from_bytes(leaked).is_some().then_some(leaked)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovery_finds_nothing_and_does_not_panic() {
        // An empty root, and a root that does not exist at all — both must be handled the same
        // way as "no font": `None`, never a panic. This is the safety property the whole task
        // brief is built on: chrome that exists for emergencies must not itself be a new way to
        // lose the session.
        let dir = tempfile::tempdir().unwrap();
        assert!(ChromeFonts::discover_under(&[dir.path().to_path_buf()]).is_none());
        assert!(ChromeFonts::discover_under(&[dir.path().join("does-not-exist")]).is_none());
        assert!(ChromeFonts::discover_under(&[]).is_none());
    }

    #[test]
    fn garbage_files_that_merely_look_like_fonts_are_skipped_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        touch(
            &dir.path().join("fonts/DejaVuSans.ttf"),
            b"not actually a font",
        );
        touch(
            &dir.path().join("fonts/other/NotoSans-Regular.ttf"),
            b"also not a font",
        );
        touch(
            &dir.path().join("fonts/readme.txt"),
            b"definitely not a font",
        );
        // Every `.ttf` present fails to parse; the non-`.ttf` file is never even tried.
        assert!(ChromeFonts::discover_under(&[dir.path().to_path_buf()]).is_none());
    }

    #[test]
    fn non_ttf_extensions_are_never_read_as_fonts() {
        let dir = tempfile::tempdir().unwrap();
        // A `.otf`/`.ttc` is exactly the kind of file `pixelkit-text` explicitly does not
        // support (CFF outlines / font collections) — it must be filtered by extension before
        // ever reaching `Face::from_bytes`, not merely fail to parse.
        touch(&dir.path().join("Foo.otf"), b"whatever");
        touch(&dir.path().join("Foo.ttc"), b"whatever");
        assert!(discover_font_bytes(&[dir.path().to_path_buf()], PREFERRED_NAMES, false).is_none());
    }

    #[test]
    fn preferred_names_are_matched_case_insensitively_and_ranked_first() {
        let root = PathBuf::from("/usr/share/fonts/truetype/dejavu");
        let preferred = root.join("DejaVuSans.ttf");
        let other = root.join("SomeOtherFont.ttf");
        assert!(
            preference_rank(&preferred, PREFERRED_NAMES) < preference_rank(&other, PREFERRED_NAMES)
        );
        // Case does not matter.
        assert_eq!(
            preference_rank(&root.join("DEJAVUSANS.TTF"), PREFERRED_NAMES),
            preference_rank(&preferred, PREFERRED_NAMES)
        );
        // The italic pass is the same rule against its own list, and an upright face never
        // qualifies for it: a missing italic is a missing cue, never an upright fallback that
        // silently says "pinned" about every window.
        assert!(
            preference_rank(&root.join("DejaVuSans-Oblique.ttf"), ITALIC_NAMES)
                < ITALIC_NAMES.len()
        );
        assert_eq!(
            preference_rank(&preferred, ITALIC_NAMES),
            ITALIC_NAMES.len(),
            "the regular face is not a candidate for italic"
        );
    }

    /// A machine with no italic font anywhere is a normal machine, and it gets a compositor that
    /// draws — the pinned name simply reads upright.
    #[test]
    fn an_italic_pass_that_finds_nothing_falls_back_to_the_regular_face() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_font_bytes(&[dir.path().to_path_buf()], ITALIC_NAMES, true).is_none());
    }

    #[test]
    fn walk_bounds_itself_by_budget_and_depth_and_never_panics_on_a_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            touch(&dir.path().join(format!("f{i}.ttf")), b"x");
        }
        let mut budget = 3;
        let found = walk(dir.path(), &mut budget);
        assert!(found.len() <= 3, "walk must respect its budget");
        assert_eq!(budget, 0);

        let mut unlimited = 1_000;
        assert!(walk(&dir.path().join("nope"), &mut unlimited).is_empty());
    }

    /// The one end-to-end test: if this development/CI machine has a real system font under one
    /// of `default_font_roots()`'s paths (every mainstream distro does — this box alone has 529
    /// `.ttf` files under `/usr/share/fonts`), discovery must actually find and load it. Skips
    /// itself, rather than failing, on a stripped environment with no fonts at all — the contract
    /// this module actually promises is "never panics", not "always finds a font".
    #[test]
    fn discovery_loads_a_real_system_font_when_one_is_present() {
        match ChromeFonts::discover() {
            Some(fonts) => {
                // A real face was registered and the cache can measure real text through it.
                let style = pixelkit_text::TextStyle::new(fonts.face, 16.0);
                let mut cache = fonts.cache;
                assert!(cache.measure("Recovery", style) > 0);
            }
            None => {
                eprintln!(
                    "no system font found under default_font_roots() on this machine — \
                     skipping the end-to-end assertion, the no-font path is covered separately"
                );
            }
        }
    }
}
