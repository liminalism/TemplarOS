//! The switcher's frozen list.
//!
//! `Alt+Tab` activates as it steps, and that activation changes both semantic MRU and visual
//! stacking. Rebuilding the list on every press can therefore make the gesture chase its own
//! mutations. The list is built once at the first press and walked until the modifier comes up;
//! this type is that frozen list, and the compositor drops it on release
//! (`templar-comp/src/input.rs`). Window eligibility and order come from semantic records, never
//! from `LayerStack`.

use crate::WindowId;

/// A switcher list and where in it the walk currently is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchRing {
    targets: Vec<WindowId>,
    /// `None` until the first step: with nothing focused the walk starts *on* the top window
    /// rather than one past it.
    index: Option<usize>,
}

impl SwitchRing {
    /// Freeze `targets` (top-most first) with the walk starting at `current`. `None` for an empty
    /// list, so a caller never has to ask whether there is anything to switch to.
    pub fn new(targets: Vec<WindowId>, current: Option<WindowId>) -> Option<Self> {
        if targets.is_empty() {
            return None;
        }
        let index = current.and_then(|id| targets.iter().position(|target| *target == id));
        Some(Self { targets, index })
    }

    /// One step forward (or backward), wrapping. The first forward step from the focused window
    /// lands on the one focused before it, which is the whole point of the chord.
    pub fn step(&mut self, forward: bool) -> WindowId {
        let len = self.targets.len();
        let next = match self.index {
            None if forward => 0,
            None => len - 1,
            Some(index) if forward => (index + 1) % len,
            Some(index) => (index + len - 1) % len,
        };
        self.index = Some(next);
        self.targets[next]
    }

    pub fn selected(&self) -> Option<WindowId> {
        self.index.map(|index| self.targets[index])
    }

    /// The frozen list, for the HUD that draws it.
    pub fn targets(&self) -> &[WindowId] {
        &self.targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(raw: &[u64]) -> Vec<WindowId> {
        raw.iter().copied().map(WindowId).collect()
    }

    #[test]
    fn the_first_step_is_the_previously_focused_window_and_the_walk_goes_on_from_there() {
        let mut ring = SwitchRing::new(ids(&[1, 2, 3]), Some(WindowId(1))).unwrap();
        assert_eq!(ring.step(true), WindowId(2));
        assert_eq!(ring.step(true), WindowId(3));
        // Wraps rather than stopping at the end.
        assert_eq!(ring.step(true), WindowId(1));
        assert_eq!(ring.step(false), WindowId(3));
        assert_eq!(ring.selected(), Some(WindowId(3)));
    }

    #[test]
    fn nothing_focused_starts_on_the_top_window_not_one_past_it() {
        let mut ring = SwitchRing::new(ids(&[1, 2, 3]), None).unwrap();
        assert_eq!(ring.step(true), WindowId(1));
        let mut backwards = SwitchRing::new(ids(&[1, 2, 3]), None).unwrap();
        assert_eq!(backwards.step(false), WindowId(3));
    }

    #[test]
    fn a_window_that_is_not_in_the_list_starts_from_the_top() {
        let mut ring = SwitchRing::new(ids(&[1, 2]), Some(WindowId(7))).unwrap();
        assert_eq!(ring.step(true), WindowId(1));
    }

    #[test]
    fn nothing_to_switch_to_is_no_ring_at_all() {
        assert!(SwitchRing::new(Vec::new(), None).is_none());
    }
}
