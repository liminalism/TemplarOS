//! Retained search rows are slots. Only fetched pages own paths; empty slots are placeholders.
use crate::file_browser::BrowseEntry;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::{Duration, Instant},
};

pub const PAGE_SIZE: u32 = 100;
#[derive(Clone, Debug)]
pub struct SearchPages {
    pub generation: u64,
    pub retained: u32,
    pub pages: BTreeMap<u32, Vec<Option<BrowseEntry>>>,
    pub pending: BTreeSet<u32>,
    pub heights: Vec<i32>,
    pub selected: Option<PathBuf>,
    pub notice: Option<String>,
    pub paused: bool,
    last_refresh: Option<Instant>,
    last_visible: (usize, usize),
    last_scroll: i32,
}
impl SearchPages {
    pub fn new(generation: u64, retained: u32, first: Vec<Option<BrowseEntry>>) -> Self {
        Self {
            generation,
            retained,
            pages: BTreeMap::from([(0, first)]),
            pending: BTreeSet::new(),
            heights: vec![30; retained as usize],
            selected: None,
            notice: None,
            paused: false,
            last_refresh: None,
            last_visible: (0, 0),
            last_scroll: 0,
        }
    }
    pub fn entry(&self, index: usize) -> Option<&BrowseEntry> {
        if index >= self.retained as usize {
            return None;
        }
        self.pages
            .get(&(index as u32 / PAGE_SIZE))?
            .get(index % PAGE_SIZE as usize)?
            .as_ref()
    }
    /// At most two admitted calls, near the visible rows, including bounded lookahead.
    pub fn request(&mut self, visible: (usize, usize)) -> Vec<u32> {
        if visible != self.last_visible {
            self.paused = false;
        }
        self.last_visible = visible;
        if self.paused || self.retained == 0 || visible.0 >= self.retained as usize {
            return Vec::new();
        }
        let first = visible.0.saturating_sub(50) / PAGE_SIZE as usize;
        let last = visible
            .1
            .saturating_add(150)
            .min(self.retained as usize)
            .div_ceil(PAGE_SIZE as usize);
        let mut missing: Vec<_> = (first as u32..last as u32)
            .filter(|page| !self.pages.contains_key(page) && !self.pending.contains(page))
            .collect();
        let center = visible.0 as u32 / PAGE_SIZE;
        missing.sort_by_key(|page| page.abs_diff(center));
        missing.truncate(2usize.saturating_sub(self.pending.len()));
        self.pending.extend(missing.iter().copied());
        missing
    }
    pub fn scrolled(&mut self, offset: i32) {
        if offset != self.last_scroll {
            self.paused = false;
            self.last_scroll = offset;
        }
    }
    pub fn accept(
        &mut self,
        generation: u64,
        page: u32,
        entries: Vec<Option<BrowseEntry>>,
    ) -> bool {
        if generation != self.generation || !self.pending.remove(&page) {
            return false;
        }
        self.pages.insert(page, entries);
        true
    }
    /// Return true for one automatic rerun. A second invalidation in one second waits for input.
    pub fn snapshot_required(&mut self, now: Instant, selected: Option<PathBuf>) -> bool {
        self.pending.clear();
        self.selected = selected;
        self.notice = Some("results refreshed — the index changed".into());
        self.paused = self
            .last_refresh
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1));
        self.last_refresh = Some(now);
        !self.paused
    }
    pub fn preserve_refresh(&mut self, old: &Self) {
        self.selected.clone_from(&old.selected);
        self.last_refresh = old.last_refresh;
        self.last_visible = old.last_visible;
        self.last_scroll = old.last_scroll;
        self.notice.clone_from(&old.notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrolling_requests_only_missing_pages_once() {
        let mut rows = SearchPages::new(1, 2000, Vec::new());
        assert_eq!(rows.request((250, 270)), vec![2, 3]);
        assert!(rows.request((250, 270)).is_empty());
        assert!(rows.accept(1, 2, Vec::new()));
        assert_eq!(rows.request((250, 270)), vec![4]);
        assert_eq!(rows.pending.len(), 2);
    }
    #[test]
    fn a_page_from_an_old_query_is_ignored() {
        let mut rows = SearchPages::new(2, 2000, Vec::new());
        rows.request((100, 120));
        assert!(!rows.accept(1, 1, Vec::new()));
        assert!(rows.pending.contains(&1));
        assert!(rows.accept(2, 1, Vec::new()));
    }
    #[test]
    fn snapshot_required_reruns_and_keeps_the_selected_path() {
        let mut old = SearchPages::new(1, 2000, Vec::new());
        let path = PathBuf::from("/native/selected");
        let now = Instant::now();
        assert!(old.snapshot_required(now, Some(path.clone())));
        let mut fresh = SearchPages::new(2, 2000, Vec::new());
        fresh.preserve_refresh(&old);
        assert_eq!(fresh.selected, Some(path.clone()));
        assert!(!fresh.snapshot_required(now + Duration::from_millis(100), Some(path)));
        assert!(fresh.request((0, 0)).is_empty());
        assert!(!fresh.request((500, 520)).is_empty());
    }
}
