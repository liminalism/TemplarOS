//! Window Shelf drawer: explicitly minimised windows, never silently vanished.
//!
//! Planning doc 01 §1: "A minimised window should go to a visible Window Shelf or Recent drawer.
//! It should never silently vanish into an icon, another workspace, or an output that is no
//! longer attached." This module is the consumer of that list.
//!
//! The shelf's data arrives over the private `templar_shell_v1` protocol
//! (`interfaces/wayland/templar-shell-v1.xml`; `crate::wayland_bridge` is the client side,
//! `compositor/templar-comp/src/protocols_shell.rs` the server). This takes `&[ShellToplevel]`
//! directly, exactly as [`crate::dock`] does, so the model and its rendering are testable against
//! fixtures with no connection at all.
//!
//! One consequence worth stating: the wire protocol carries a `shelf_rank`
//! ("the Window Shelf's authoritative most-recently-shelved-first sort key", so the shell never
//! has to track shelve order itself), but `templar_window::shell::ShellToplevel` — the sanitized
//! struct this crate actually receives — has no such field today. Rather than invent a shelve
//! order the shell is explicitly not supposed to own, this sorts by `window_id` for a stable,
//! deterministic listing and leaves the real ordering to whoever wires up `shelf_rank`.

use crate::drawers::DrawerFeed;
use templar_window::ShellToplevel;

/// Every shelved window, in a stable order (see the module note on `shelf_rank`).
pub fn shelved(toplevels: &[ShellToplevel]) -> Vec<&ShellToplevel> {
    let mut shelved: Vec<&ShellToplevel> = toplevels
        .iter()
        .filter(|toplevel| toplevel.shelved)
        .collect();
    shelved.sort_by_key(|toplevel| toplevel.window_id.0);
    shelved
}

pub fn render(toplevels: &[ShellToplevel]) -> String {
    feed(toplevels).to_text()
}

pub fn feed(toplevels: &[ShellToplevel]) -> DrawerFeed {
    let shelved = shelved(toplevels);
    if shelved.is_empty() {
        return DrawerFeed::vacant("nothing shelved");
    }
    let headline = format!(
        "{} shelved",
        DrawerFeed::count(shelved.len(), "window", "windows", "nothing")
    );
    let lines = shelved
        .into_iter()
        .map(|toplevel| {
            format!(
                "{:<24} {:<32} {}",
                toplevel
                    .canonical_app_id
                    .as_ref()
                    .map_or_else(|| "(unidentified)".to_owned(), ToString::to_string),
                if toplevel.title.is_empty() {
                    "(untitled)"
                } else {
                    &toplevel.title
                },
                if toplevel.verified_identity {
                    "verified"
                } else {
                    "unverified identity"
                },
            )
        })
        .collect();
    DrawerFeed::new(headline, lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_window::{Layer, WindowId};

    fn toplevel(id: u64, app: Option<&str>, title: &str, shelved: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: app.map(|a| a.parse().unwrap()),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: app.is_some(),
            activated: false,
            mru_rank: 0,
            fullscreen: false,
            shelved,
            maximized: false,
            geometry: templar_window::Rect::new(0, 0, 640, 480),
            layer: Layer::Normal,
            group: None,
        }
    }

    #[test]
    fn only_shelved_windows_appear() {
        let windows = [
            toplevel(1, Some("org.templaros.Editor"), "report.md", true),
            toplevel(2, Some("org.templaros.Viewer"), "scan.pdf", false),
        ];
        let shelf = shelved(&windows);
        assert_eq!(shelf.len(), 1);
        assert_eq!(shelf[0].window_id, WindowId(1));

        let text = render(&windows);
        assert!(text.contains("report.md"));
        assert!(
            !text.contains("scan.pdf"),
            "an unshelved window leaked into the shelf"
        );
    }

    #[test]
    fn the_order_is_stable_regardless_of_input_order() {
        let ascending = [
            toplevel(1, Some("org.templaros.Editor"), "a", true),
            toplevel(2, Some("org.templaros.Editor"), "b", true),
        ];
        let descending = [
            toplevel(2, Some("org.templaros.Editor"), "b", true),
            toplevel(1, Some("org.templaros.Editor"), "a", true),
        ];
        assert_eq!(render(&ascending), render(&descending));
    }

    #[test]
    fn an_unidentified_window_is_still_recoverable_from_the_shelf() {
        // The whole point of the shelf is that a window cannot become lost — including one whose
        // identity the compositor could not verify.
        let windows = [toplevel(7, None, "", true)];
        let text = render(&windows);
        assert!(text.contains("(unidentified)"));
        assert!(text.contains("(untitled)"));
        assert!(text.contains("unverified identity"));
    }

    #[test]
    fn nothing_shelved_is_said_plainly() {
        assert_eq!(render(&[]), "nothing shelved");
        let unshelved = [toplevel(1, Some("org.templaros.Editor"), "a", false)];
        assert_eq!(render(&unshelved), "nothing shelved");
    }
}
