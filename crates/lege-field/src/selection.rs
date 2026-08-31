//! Ordered multi-selection shared by Home tiles and drawer entries.
//!
//! Focus answers where keyboard navigation is; selection answers what an operation applies to.
//! They are deliberately separate. The anchor is the stable end of a Shift range, while the
//! ordered set makes every bulk operation deterministic regardless of click order.

use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection<T: Ord> {
    selected: BTreeSet<T>,
    anchor: Option<T>,
}

impl<T: Ord> Default for Selection<T> {
    fn default() -> Self {
        Self {
            selected: BTreeSet::new(),
            anchor: None,
        }
    }
}

impl<T: Clone + Ord> Selection<T> {
    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    pub fn replace(&mut self, item: T) {
        self.selected.clear();
        self.selected.insert(item.clone());
        self.anchor = Some(item);
    }

    /// Replace the selection with an arbitrary inventory while retaining the first item as the
    /// range anchor. This is used by rectangular selection, whose geometry need not be a
    /// contiguous slice of reading order.
    pub fn replace_many(&mut self, items: impl IntoIterator<Item = T>) {
        let items: Vec<T> = items.into_iter().collect();
        self.anchor = items.first().cloned();
        self.selected = items.into_iter().collect();
    }

    pub fn toggle(&mut self, item: T) {
        if !self.selected.remove(&item) {
            self.selected.insert(item.clone());
        }
        self.anchor = Some(item);
    }

    /// Replace the selection with the inclusive range from the stable anchor to `target` in
    /// `order`. If the anchor disappeared during a refresh, `target` becomes the new anchor.
    pub fn extend_to(&mut self, order: &[T], target: T) {
        let Some(target_index) = order.iter().position(|item| item == &target) else {
            return;
        };
        let anchor = self.anchor.clone().unwrap_or_else(|| target.clone());
        let Some(anchor_index) = order.iter().position(|item| item == &anchor) else {
            self.replace(target);
            return;
        };
        let (start, end) = if anchor_index <= target_index {
            (anchor_index, target_index)
        } else {
            (target_index, anchor_index)
        };
        self.selected = order[start..=end].iter().cloned().collect();
        self.anchor = Some(anchor);
    }

    pub fn select_all(&mut self, order: &[T]) {
        self.selected = order.iter().cloned().collect();
        self.anchor = order.first().cloned();
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.selected.retain(|item| keep(item));
        if self.anchor.as_ref().is_some_and(|item| !keep(item)) {
            self.anchor = None;
        }
    }

    pub fn contains(&self, item: &T) -> bool {
        self.selected.contains(item)
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.selected.iter()
    }

    pub fn to_vec(&self) -> Vec<T> {
        self.selected.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_is_a_set_and_updates_the_range_anchor() {
        let mut selection = Selection::default();
        selection.toggle(3);
        selection.toggle(1);
        selection.toggle(3);
        assert_eq!(selection.to_vec(), vec![1]);
        selection.extend_to(&[1, 2, 3, 4], 4);
        assert_eq!(selection.to_vec(), vec![3, 4]);
    }

    #[test]
    fn a_shift_range_is_inclusive_in_either_direction() {
        let order = [10, 20, 30, 40];
        let mut selection = Selection::default();
        selection.replace(30);
        selection.extend_to(&order, 10);
        assert_eq!(selection.to_vec(), vec![10, 20, 30]);
        selection.extend_to(&order, 40);
        assert_eq!(selection.to_vec(), vec![30, 40]);
    }

    #[test]
    fn select_all_and_retain_follow_the_live_inventory() {
        let mut selection = Selection::default();
        selection.select_all(&[3, 1, 2]);
        assert_eq!(selection.to_vec(), vec![1, 2, 3]);
        selection.retain(|item| *item != 2);
        assert_eq!(selection.to_vec(), vec![1, 3]);
        selection.clear();
        assert!(selection.is_empty());
    }
}
