use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    fs::{self, File, ReadDir},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use templar_types::wallpaper::{SourceOrder, WallpaperConfig};

const MAX_ENTRIES: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
const MAX_SPOOL_BYTES: u64 = 256 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Next,
    Previous,
}
#[derive(Debug)]
struct Shuffle {
    records: File,
    index: File,
    count: u64,
    cursor: u64,
    roots: Vec<Vec<u8>>,
    recursive: bool,
}
#[derive(Debug)]
pub struct SourceSelector {
    random: u64,
    history: VecDeque<PathBuf>,
    history_cursor: Option<usize>,
    pending_history: Option<usize>,
    shuffle: Option<Shuffle>,
    spool_directory: PathBuf,
}
impl Default for SourceSelector {
    fn default() -> Self {
        Self::new()
    }
}
impl SourceSelector {
    pub fn new() -> Self {
        Self::new_in(PathBuf::from("/var/tmp"))
    }
    /// Use an existing writable directory for anonymous shuffle metadata files. Native
    /// sessiond supplies its already-owned StateDirectory without changing /tmp visibility.
    pub fn new_in(spool_directory: PathBuf) -> Self {
        let mut seed = [0_u8; 8];
        // A failed entropy read remains an explicit error at selection, not a fixed RNG fallback.
        let random = File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut seed))
            .map_or(0, |()| u64::from_ne_bytes(seed).max(1));
        Self {
            random,
            history: VecDeque::new(),
            history_cursor: None,
            pending_history: None,
            shuffle: None,
            spool_directory,
        }
    }
    /// Only successful application belongs in navigation history; never record a trial decode.
    pub fn record_success(&mut self, path: &Path) {
        if let Some(position) = self.pending_history.take()
            && self.history.get(position).is_some_and(|old| old == path)
        {
            self.history_cursor = Some(position);
            return;
        }
        if let Some(position) = self.history_cursor {
            if self.history.get(position).is_some_and(|old| old == path) {
                return;
            }
            self.history.truncate(position + 1);
        }
        if self.history.len() == 64 {
            self.history.pop_front();
        }
        self.history.push_back(path.to_owned());
        self.history_cursor = Some(self.history.len() - 1);
    }
    pub fn refresh(&mut self) {
        self.shuffle = None;
    }
    pub fn select(
        &mut self,
        config: &WallpaperConfig,
        previous: Option<&Path>,
        direction: Direction,
    ) -> Result<PathBuf> {
        self.select_cancellable(config, previous, direction, || false)
    }
    pub fn select_cancellable(
        &mut self,
        config: &WallpaperConfig,
        previous: Option<&Path>,
        direction: Direction,
        cancelled: impl Fn() -> bool,
    ) -> Result<PathBuf> {
        ensure!(!cancelled(), "wallpaper source selection cancelled");
        crate::config::validate(config)?;
        ensure!(
            config.order == SourceOrder::Sequential || self.random != 0,
            "wallpaper random entropy unavailable"
        );
        self.pending_history = None;
        if direction == Direction::Previous
            && let Some(current) = previous
        {
            let position = self
                .history_cursor
                .filter(|&position| {
                    self.history
                        .get(position)
                        .is_some_and(|path| path == current)
                })
                .or_else(|| self.history.iter().rposition(|path| path == current));
            if let Some(position) = position
                && position > 0
            {
                self.pending_history = Some(position - 1);
                return Ok(self.history[position - 1].clone());
            }
        }
        match config.order {
            SourceOrder::Sequential => {
                Self::select_sequential(config, previous, direction, &cancelled)
            }
            SourceOrder::Random => self.select_random(config, previous, &cancelled),
            SourceOrder::Shuffle => {
                let rebuild = self.shuffle.as_ref().is_none_or(|state| {
                    state.roots != config.sources
                        || state.recursive != config.recursive
                        || state.cursor >= state.count
                });
                if rebuild {
                    // Anonymous private disk files: no unbounded filename vector or visited set.
                    let mut records = tempfile::tempfile_in(&self.spool_directory)
                        .context("create shuffle path spool")?;
                    let mut index = tempfile::tempfile_in(&self.spool_directory)
                        .context("create shuffle permutation")?;
                    let mut count = 0_u64;
                    visit(config, &cancelled, |path| {
                        let bytes = path.as_os_str().as_bytes();
                        let position = records.stream_position()?;
                        ensure!(
                            position + 4 + bytes.len() as u64 <= MAX_SPOOL_BYTES,
                            "shuffle metadata exceeds 256 MiB disk envelope"
                        );
                        index.write_all(&position.to_le_bytes())?;
                        records.write_all(&(bytes.len() as u32).to_le_bytes())?;
                        records.write_all(bytes)?;
                        count += 1;
                        Ok(())
                    })?;
                    ensure!(count > 0, "no supported wallpaper sources");
                    let shuffle_started = Instant::now();
                    for i in (1..count).rev() {
                        ensure!(!cancelled(), "wallpaper shuffle cancelled");
                        ensure!(
                            shuffle_started.elapsed() <= Duration::from_secs(30),
                            "shuffle permutation exceeds 30-second work envelope"
                        );
                        let j = self.below(i + 1);
                        let a = read_offset(&mut index, i)?;
                        let b = read_offset(&mut index, j)?;
                        write_offset(&mut index, i, b)?;
                        write_offset(&mut index, j, a)?;
                    }
                    // Keep cycle boundaries from repeating the last successfully applied source.
                    if count > 1
                        && previous.is_some_and(|old| {
                            read_path(&mut records, &mut index, 0).is_ok_and(|path| path == old)
                        })
                    {
                        let j = 1 + self.below(count - 1);
                        let a = read_offset(&mut index, 0)?;
                        let b = read_offset(&mut index, j)?;
                        write_offset(&mut index, 0, b)?;
                        write_offset(&mut index, j, a)?;
                    }
                    self.shuffle = Some(Shuffle {
                        records,
                        index,
                        count,
                        cursor: 0,
                        roots: config.sources.clone(),
                        recursive: config.recursive,
                    });
                }
                let state = self.shuffle.as_mut().expect("initialized");
                let path = read_path(&mut state.records, &mut state.index, state.cursor)?;
                state.cursor += 1;
                Ok(path)
            }
        }
    }
    fn select_sequential(
        config: &WallpaperConfig,
        previous: Option<&Path>,
        direction: Direction,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PathBuf> {
        let mut first: Option<PathBuf> = None;
        let mut selected: Option<PathBuf> = None;
        visit(config, cancelled, |path| {
            let before = direction == Direction::Previous;
            let better = |a: &Path, b: &Path| if before { a > b } else { a < b };
            if first.as_ref().is_none_or(|old| better(&path, old)) {
                first = Some(path.clone());
            }
            if previous.is_none_or(|old| {
                if before {
                    path.as_path() < old
                } else {
                    path.as_path() > old
                }
            }) && selected.as_ref().is_none_or(|old| better(&path, old))
            {
                selected = Some(path);
            }
            Ok(())
        })?;
        selected.or(first).context("no supported wallpaper sources")
    }
    fn select_random(
        &mut self,
        config: &WallpaperConfig,
        previous: Option<&Path>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PathBuf> {
        let mut count = 0;
        let mut choice = None;
        let mut repeat = None;
        visit(config, cancelled, |path| {
            if previous == Some(path.as_path()) {
                repeat = Some(path);
                return Ok(());
            }
            count += 1;
            if self.below(count) == 0 {
                choice = Some(path);
            }
            Ok(())
        })?;
        choice.or(repeat).context("no supported wallpaper sources")
    }
    fn below(&mut self, bound: u64) -> u64 {
        // SplitMix64 plus rejection removes modulo bias in reservoir/Fisher-Yates sampling.
        let threshold = bound.wrapping_neg() % bound;
        loop {
            self.random = self.random.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = self.random;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^= value >> 31;
            if value >= threshold {
                return value % bound;
            }
        }
    }
}
fn read_offset(file: &mut File, index: u64) -> Result<u64> {
    file.seek(SeekFrom::Start(index * 8))?;
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
fn write_offset(file: &mut File, index: u64, value: u64) -> Result<()> {
    file.seek(SeekFrom::Start(index * 8))?;
    file.write_all(&value.to_le_bytes())?;
    Ok(())
}
fn read_path(records: &mut File, index: &mut File, cursor: u64) -> Result<PathBuf> {
    records.seek(SeekFrom::Start(read_offset(index, cursor)?))?;
    let mut length = [0; 4];
    records.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= 4096, "invalid shuffle path record");
    let mut bytes = vec![0; length];
    records.read_exact(&mut bytes)?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}
fn supported(path: &Path) -> bool {
    path.extension().is_some_and(|value| {
        [b"png".as_slice(), b"jpg", b"jpeg", b"webp"]
            .iter()
            .any(|extension| value.as_bytes().eq_ignore_ascii_case(extension))
    })
}
fn visit(
    config: &WallpaperConfig,
    cancelled: &dyn Fn() -> bool,
    mut candidate: impl FnMut(PathBuf) -> Result<()>,
) -> Result<()> {
    let started = Instant::now();
    let mut work = 0;
    let mut roots: Vec<(PathBuf, bool)> = Vec::new();
    for bytes in &config.sources {
        ensure!(!cancelled(), "wallpaper source traversal cancelled");
        let raw = PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()));
        // Normalize overlapping roots without a candidate-sized deduplication set.
        let root = fs::canonicalize(&raw)
            .with_context(|| format!("open source root {}", raw.display()))?;
        let metadata = fs::symlink_metadata(&raw)?;
        if metadata.file_type().is_symlink() && root.is_dir() {
            bail!(
                "symlink source directories are not followed: {}",
                raw.display()
            );
        }
        let directory = metadata.is_dir();
        if roots.iter().any(|(old, old_dir)| {
            *old == root
                || (!directory
                    && *old_dir
                    && root
                        .strip_prefix(old)
                        .is_ok_and(|rest| config.recursive || rest.components().count() == 1))
                || (directory && config.recursive && *old_dir && root.starts_with(old))
        }) {
            continue;
        }
        if directory {
            roots.retain(|(old, old_dir)| {
                !old.strip_prefix(&root).is_ok_and(|rest| {
                    config.recursive || (!*old_dir && rest.components().count() == 1)
                })
            });
        }
        roots.push((root, directory));
    }
    for (root, directory) in &roots {
        ensure!(!cancelled(), "wallpaper source traversal cancelled");
        if !directory {
            if root.is_file() {
                candidate(root.clone())?;
            }
            continue;
        }
        let mut stack: Vec<ReadDir> =
            vec![fs::read_dir(root).with_context(|| format!("scan {}", root.display()))?];
        while let Some(entries) = stack.last_mut() {
            ensure!(!cancelled(), "wallpaper source traversal cancelled");
            let Some(entry) = entries.next() else {
                stack.pop();
                continue;
            };
            work += 1;
            ensure!(
                work <= MAX_ENTRIES && started.elapsed() <= Duration::from_secs(30),
                "source traversal exceeded one million entries or 30 seconds; split source roots"
            );
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() && config.recursive {
                ensure!(
                    stack.len() < MAX_DEPTH,
                    "source traversal exceeds 64 directory levels"
                );
                stack.push(fs::read_dir(entry.path())?);
            } else if supported(&entry.path())
                && (kind.is_file()
                    || (kind.is_symlink()
                        && fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_file())))
            {
                candidate(entry.path())?;
            }
            // Symlink directories are never followed, including recursive traversal.
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sequence_and_shuffle_are_lossless() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join(std::ffi::OsString::from_vec(vec![
            b'a', 0xff, b'.', b'p', b'n', b'g',
        ]));
        let b = directory.path().join("b.jpg");
        File::create(&a).unwrap();
        File::create(&b).unwrap();
        let mut config = WallpaperConfig {
            sources: vec![directory.path().as_os_str().as_bytes().to_vec()],
            order: SourceOrder::Sequential,
            ..WallpaperConfig::default()
        };
        let mut selector = SourceSelector::new();
        assert_eq!(selector.select(&config, None, Direction::Next).unwrap(), a);
        assert_eq!(
            selector.select(&config, Some(&a), Direction::Next).unwrap(),
            b
        );
        config.order = SourceOrder::Shuffle;
        let first = selector.select(&config, None, Direction::Next).unwrap();
        let second = selector
            .select(&config, Some(&first), Direction::Next)
            .unwrap();
        assert_ne!(first, second);
    }
    #[test]
    fn cancelled_jobs_and_previous_history_are_bounded() {
        let mut selector = SourceSelector::new();
        let config = WallpaperConfig::default();
        assert!(
            selector
                .select_cancellable(&config, None, Direction::Next, || true)
                .is_err()
        );
        for path in ["a.png", "b.png", "c.png"] {
            selector.record_success(Path::new(path));
        }
        let previous = selector
            .select(&config, Some(Path::new("c.png")), Direction::Previous)
            .unwrap();
        assert_eq!(previous, Path::new("b.png"));
        selector.record_success(&previous);
        assert_eq!(
            selector
                .select(&config, Some(&previous), Direction::Previous)
                .unwrap(),
            Path::new("a.png")
        );
    }
    #[test]
    fn shuffle_uses_the_supplied_spool_directory() {
        let sources = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        let image = sources.path().join("source.png");
        File::create(&image).unwrap();
        let config = WallpaperConfig {
            sources: vec![sources.path().as_os_str().as_bytes().to_vec()],
            order: SourceOrder::Shuffle,
            ..WallpaperConfig::default()
        };
        let mut selector = SourceSelector::new_in(spool.path().to_owned());
        assert_eq!(
            selector.select(&config, None, Direction::Next).unwrap(),
            image
        );
        let mut unavailable = SourceSelector::new_in(spool.path().join("not-created"));
        assert!(unavailable.select(&config, None, Direction::Next).is_err());
    }
}
