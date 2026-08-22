//! File categories: the short list colour and composition are read in, and the classification
//! of a path into one (basename rules, compound suffixes, then the extension).

use std::path::Path;

/// High-level file categories used everywhere in the UI.
/// Keep this list short so colors remain immediately readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileCategory {
    Document,
    Code,
    Image,
    Video,
    Audio,
    Archive,
    System,
    Misc,
}

pub const CATEGORY_COUNT: usize = 8;

pub const CATEGORY_ORDER: [FileCategory; CATEGORY_COUNT] = [
    FileCategory::Document,
    FileCategory::Code,
    FileCategory::Image,
    FileCategory::Video,
    FileCategory::Audio,
    FileCategory::Archive,
    FileCategory::System,
    FileCategory::Misc,
];

pub const fn category_index(category: FileCategory) -> usize {
    match category {
        FileCategory::Document => 0,
        FileCategory::Code => 1,
        FileCategory::Image => 2,
        FileCategory::Video => 3,
        FileCategory::Audio => 4,
        FileCategory::Archive => 5,
        FileCategory::System => 6,
        FileCategory::Misc => 7,
    }
}

pub const fn category_label(category: FileCategory) -> &'static str {
    match category {
        FileCategory::Document => "Documents",
        FileCategory::Code => "Code",
        FileCategory::Image => "Images",
        FileCategory::Video => "Video",
        FileCategory::Audio => "Audio",
        FileCategory::Archive => "Archives",
        FileCategory::System => "System",
        FileCategory::Misc => "Misc",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathClassification {
    pub normalized_extension: Option<String>,
    pub category: FileCategory,
}

const BASENAME_RULES: &[(&str, FileCategory)] = &[
    (".editorconfig", FileCategory::System),
    (".env", FileCategory::System),
    (".gitattributes", FileCategory::System),
    (".gitignore", FileCategory::System),
    ("cargo.lock", FileCategory::System),
    ("cargo.toml", FileCategory::System),
    ("changelog", FileCategory::Document),
    ("cmakelists.txt", FileCategory::Code),
    ("copying", FileCategory::Document),
    ("dockerfile", FileCategory::Code),
    ("license", FileCategory::Document),
    ("makefile", FileCategory::Code),
    ("readme", FileCategory::Document),
    ("readme.txt", FileCategory::Document),
    ("readme.md", FileCategory::Document),
];

const COMPOUND_SUFFIX_RULES: &[(&str, FileCategory)] = &[
    ("d.ts", FileCategory::Code),
    ("spec.ts", FileCategory::Code),
    ("tar.bz2", FileCategory::Archive),
    ("tar.gz", FileCategory::Archive),
    ("tar.xz", FileCategory::Archive),
    ("tar.zst", FileCategory::Archive),
    ("test.ts", FileCategory::Code),
    ("user.css", FileCategory::Code),
    ("user.js", FileCategory::Code),
];

/// Fast path for normalized lowercase extensions.
pub fn categorize_extension(ext: &str) -> FileCategory {
    let ext = ext.trim_start_matches('.');
    if ext.is_empty() {
        return FileCategory::Misc;
    }
    match ext {
        "3fr" | "ai" | "arw" | "avif" | "blend" | "bmp" | "cr2" | "cr3" | "dae" | "dcm" | "dcr"
        | "dng" | "erf" | "fbx" | "gif" | "glb" | "gltf" | "heic" | "ico" | "iiq" | "jpeg"
        | "jpg" | "kdc" | "mef" | "mos" | "mrw" | "nef" | "nrw" | "orf" | "pef" | "png" | "psb"
        | "psd" | "raf" | "raw" | "rw2" | "sr2" | "stl" | "svg" | "tif" | "tiff" | "usd"
        | "usdz" | "webp" | "x3f" => FileCategory::Image,

        "3gp" | "avi" | "flv" | "m2ts" | "m4v" | "mkv" | "mov" | "mp4" | "mpeg" | "mpg" | "mts"
        | "ogv" | "rm" | "rmvb" | "webm" | "wmv" => FileCategory::Video,

        "aac" | "aif" | "aiff" | "alac" | "ape" | "flac" | "m4a" | "mid" | "midi" | "mp3"
        | "ogg" | "opus" | "wav" | "wma" => FileCategory::Audio,

        "azw3" | "changelog" | "csv" | "djv" | "djvu" | "doc" | "docx" | "epub" | "indd"
        | "log" | "markdown" | "md" | "mobi" | "numbers" | "odp" | "ods" | "odt" | "org"
        | "pages" | "pdf" | "ppt" | "pptx" | "rst" | "rtf" | "tex" | "tsv" | "txt" | "xls"
        | "xlsx" => FileCategory::Document,

        "7z" | "bak" | "bz2" | "cab" | "dmg" | "gz" | "img" | "iso" | "lz" | "lz4" | "lzma"
        | "old" | "qcow2" | "rar" | "tar" | "tbz2" | "tgz" | "txz" | "vhd" | "vhdx" | "vmdk"
        | "xz" | "zip" | "zst" => FileCategory::Archive,

        "a" | "asm" | "bash" | "c" | "cc" | "class" | "clj" | "cljs" | "cmake" | "cpp" | "cs"
        | "dart" | "d" | "erl" | "ex" | "exs" | "exp" | "filters" | "fish" | "fs" | "fsi"
        | "fsx" | "go" | "gradle" | "h" | "hh" | "hpp" | "hrl" | "htm" | "html" | "idb" | "ilk"
        | "jar" | "java" | "js" | "jsx" | "kt" | "less" | "lib" | "lockb" | "lua" | "m"
        | "make" | "mk" | "ml" | "mli" | "nar" | "nim" | "o" | "obj" | "pdb" | "pch" | "php"
        | "pl" | "pm" | "psql" | "py" | "r" | "rb" | "rlib" | "rmeta" | "rs" | "s" | "scala"
        | "scss" | "sh" | "sln" | "spec" | "sql" | "svelte" | "swift" | "ts" | "tsx"
        | "vcxproj" | "vue" | "war" | "wasm" | "zig" | "zsh" => FileCategory::Code,

        "accdb" | "appimage" | "bat" | "cer" | "cfg" | "cmd" | "com" | "conf" | "crt" | "db"
        | "deb" | "desktop" | "dll" | "dylib" | "elf" | "env" | "eot" | "exe" | "ini" | "json"
        | "lnk" | "lock" | "manifest" | "mobileconfig" | "msi" | "otf" | "pem" | "pkg"
        | "plist" | "properties" | "ps1" | "reg" | "rpm" | "scr" | "service" | "so" | "sqlite"
        | "sqlite3" | "sys" | "target" | "toml" | "ttf" | "woff" | "woff2" | "xml" | "yaml"
        | "yml" => FileCategory::System,

        _ => FileCategory::Misc,
    }
}

/// Classify a path once during indexing and cache the result in file metadata.
pub fn classify_path(path: &Path) -> PathClassification {
    let Some(file_name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return PathClassification {
            normalized_extension: None,
            category: FileCategory::Misc,
        };
    };

    if let Some(category) = categorize_basename(file_name.as_ref()) {
        return PathClassification {
            normalized_extension: None,
            category,
        };
    }

    let file_name_lower = file_name.to_ascii_lowercase();
    if let Some((suffix, category)) = categorize_compound_suffix(&file_name_lower) {
        return PathClassification {
            normalized_extension: Some(suffix.to_string()),
            category,
        };
    }

    let normalized_extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map(str::to_ascii_lowercase);

    let category = normalized_extension
        .as_deref()
        .map_or(FileCategory::Misc, categorize_extension);

    PathClassification {
        normalized_extension,
        category,
    }
}

fn categorize_basename(file_name: &str) -> Option<FileCategory> {
    BASENAME_RULES
        .iter()
        .find(|(name, _)| file_name.eq_ignore_ascii_case(name))
        .map(|(_, category)| *category)
}

fn categorize_compound_suffix(file_name_lower: &str) -> Option<(&'static str, FileCategory)> {
    COMPOUND_SUFFIX_RULES
        .iter()
        .find(|(suffix, _)| file_name_lower.ends_with(suffix))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_requested_extensions() {
        assert_eq!(categorize_extension("psd"), FileCategory::Image);
        assert_eq!(categorize_extension("arw"), FileCategory::Image);
        assert_eq!(categorize_extension("dcm"), FileCategory::Image);
        assert_eq!(categorize_extension("indd"), FileCategory::Document);
        assert_eq!(categorize_extension("d"), FileCategory::Code);
    }

    #[test]
    fn handles_basename_rules() {
        assert_eq!(
            classify_path(Path::new("Dockerfile")).category,
            FileCategory::Code
        );
        assert_eq!(
            classify_path(Path::new(".gitignore")).category,
            FileCategory::System
        );
        assert_eq!(
            classify_path(Path::new("README")).category,
            FileCategory::Document
        );
    }

    #[test]
    fn handles_compound_suffixes() {
        let archive = classify_path(Path::new("backup.tar.gz"));
        assert_eq!(archive.normalized_extension.as_deref(), Some("tar.gz"));
        assert_eq!(archive.category, FileCategory::Archive);

        let code = classify_path(Path::new("types.d.ts"));
        assert_eq!(code.normalized_extension.as_deref(), Some("d.ts"));
        assert_eq!(code.category, FileCategory::Code);
    }
}
