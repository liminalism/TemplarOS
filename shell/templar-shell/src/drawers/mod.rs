//! Drawers as live views over the services, per `docs/planning/01-initial-shape.md`'s Home
//! Surface table. Each module: a small async client trait (implemented for real directly on the
//! matching `templar_client::proxies::*Proxy` type, and by a fake in its own tests), and a `render`
//! function that turns the service's answer into the lines the drawer shows.
//!
//! `crates/templar-window` (`shell.rs`, `layers.rs`) and this crate's own [`crate::home_surface`]
//! own the *set* of drawers; nothing here invents a new one.

pub mod network;
pub mod operations;
pub mod projects;
pub mod recent;
pub mod standard_paths;
pub mod trash;
pub mod volumes;
pub mod window_shelf;

/// One drawer's contents at two densities: a line worth reading at a glance, and the full
/// listing behind it.
///
/// The Home Field draws tiles at whatever size they have been given, and a drawer used to receive
/// only its listing — so a tile too short for the list showed the first line or two of it, clipped
/// mid-path. `@templaros.decision.home-field-stable-bounded-semantic-grammar` requires the opposite: when a tile becomes too small for one
/// view, it chooses a simpler view rather than merely shrinking text and controls", with a file
/// tray's compact state being its name and count. The system widgets already worked this way
/// through `WidgetSnapshot`; drawers had no equivalent, so they could not climb the same ladder.
///
/// The headline is built where the structured data is — a count of volumes, of recent items, of
/// files in a directory — and never parsed back out of the rendered text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrawerFeed {
    /// The count or figure the drawer is *about*, in a few words. Shown at every rung.
    pub headline: String,
    /// The listing, one entry per line. Shown from `Representation::Wide` upward.
    pub lines: Vec<String>,
    /// For a drawer whose rows name something the shell can act on, the target of the row at the
    /// same index — a trashed item's path, for the Trash drawer's restore-on-click.
    ///
    /// Empty for every other drawer, and kept beside the lines rather than parsed back out of
    /// them: a rendered row is for a person to read and a file name may contain anything, so
    /// recovering a path from one would be a guess. Same rule as the headline.
    pub row_targets: Vec<Option<String>>,
    /// Whether the backing service answered. A drawer that cannot reach its service says so
    /// rather than showing a plausible zero.
    pub available: bool,
    /// Whether this feed is an empty collection. Structured so a surface does not have to parse
    /// the headline for "empty" / "nothing" / "nothing yet".
    pub empty: bool,
    /// A running operation's label and percent, when anything is still in flight. Structured so
    /// the Files object does not have to recover progress from rendered text.
    pub progress: Option<(String, u8)>,
}

impl DrawerFeed {
    pub fn new(headline: impl Into<String>, lines: Vec<String>) -> DrawerFeed {
        DrawerFeed {
            headline: headline.into(),
            lines,
            row_targets: Vec::new(),
            available: true,
            empty: false,
            progress: None,
        }
    }

    /// An empty collection, with the word the drawer uses for that. Distinct from
    /// [`Self::unavailable`]: nothing failed, there is simply nothing here.
    pub fn vacant(headline: impl Into<String>) -> DrawerFeed {
        DrawerFeed {
            headline: headline.into(),
            lines: Vec::new(),
            row_targets: Vec::new(),
            available: true,
            empty: true,
            progress: None,
        }
    }

    /// Attach in-flight progress. The Files object reads this instead of parsing the listing.
    #[must_use]
    pub fn with_progress(mut self, label: impl Into<String>, percent: u8) -> DrawerFeed {
        self.progress = Some((label.into(), percent));
        self
    }

    /// What clicking the row at `index` should act on, if anything.
    pub fn row_target(&self, index: usize) -> Option<&str> {
        self.row_targets.get(index)?.as_deref()
    }

    /// A feed for a service that did not answer, where the whole sentence is the headline.
    ///
    /// Prefer [`Self::unreachable`] when there is a reason worth keeping: a headline is drawn on
    /// one line at the tile's width, so a long one is truncated with an ellipsis, and the part
    /// that gets cut is the end — which is exactly where the reason is.
    pub fn unavailable(reason: impl Into<String>) -> DrawerFeed {
        DrawerFeed {
            headline: reason.into(),
            lines: Vec::new(),
            row_targets: Vec::new(),
            available: false,
            empty: false,
            progress: None,
        }
    }

    /// A service that did not answer, with the short fact in the headline and the reason under
    /// it.
    ///
    /// "Drives — service unavailable: no reply from Files.Volumes1" put in a headline came out as
    /// "Drives — service unavaila…", which tells a person nothing they did not already know from
    /// looking at an empty tile. The subject fits at every rung; the reason gets a line of its
    /// own, where there is room to read it.
    pub fn unreachable(subject: impl Into<String>, reason: impl Into<String>) -> DrawerFeed {
        DrawerFeed {
            headline: format!("{} — unavailable", subject.into()),
            lines: vec![reason.into()],
            row_targets: Vec::new(),
            available: false,
            empty: false,
            progress: None,
        }
    }

    /// `n item(s)`, or `empty` for none — the shape most drawers' headlines take.
    pub fn count(n: usize, singular: &str, plural: &str, empty: &str) -> String {
        match n {
            0 => empty.to_owned(),
            1 => format!("1 {singular}"),
            n => format!("{n} {plural}"),
        }
    }

    /// The whole feed as text, which is what `templar-shell --print <drawer>` shows: the headline
    /// first, then the listing.
    pub fn to_text(&self) -> String {
        if self.lines.is_empty() {
            return self.headline.clone();
        }
        let mut text = String::with_capacity(self.headline.len() + self.lines.len() * 32);
        text.push_str(&self.headline);
        for line in &self.lines {
            text.push('\n');
            text.push_str(line);
        }
        text
    }
}
