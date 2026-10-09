//! Tiles: what one is, how big it may be, and how much it shows at a given size.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use templar_types::AppId;

/// A tile's identity, stable for the document's lifetime — never reused.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TileId(pub u64);

impl TileId {
    /// The id a named object always has, so opening the same thing twice finds the tile that is
    /// already there. FNV-1a, 64-bit; collisions are astronomically unlikely for a few hundred
    /// keys, and `0` is reserved.
    #[must_use]
    pub fn for_key(key: &str) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in key.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        Self(hash.max(1))
    }
}

/// What a tile stands for. The outer shape is always a rectangle; type is expressed through
/// internal structure and behaviour
/// (`@templaros.decision.home-field-stable-bounded-semantic-grammar`), so this is the one
/// place the kinds are enumerated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TileKind {
    /// A container: files, tools, sessions and scoped widgets that belong together. Refers to a
    /// `Projects1` project; holds other tiles one level deep.
    Project { project_id: String },
    /// A window group's tile: the `ParkedSession` tiles of its members, held together as one
    /// thing to move, park and return.
    ///
    /// A container like `Project`, and top level only for the same reason — containers stay one
    /// deep. It is a *representation*: `templar_window` owns membership and the compositor is the
    /// source of truth (`docs/planning/window-groups-and-application-integration.md` §2 rule 2),
    /// so nothing here creates or dissolves a group. `identifier` is the group's id as a decimal
    /// string, the same shape `ParkedSession::identifier` uses for a window.
    WindowGroup { identifier: String },
    /// A container of application shortcuts: the favourites a person put together by dropping
    /// one app tile on another. Shown as a grid of icons, each a launch; opened like a project to
    /// rearrange. Holds `App` tiles only, one level deep like every container, and nothing on
    /// disk corresponds to it — unlike a project, it is a Home arrangement and nothing more.
    AppGroup { group_id: String },
    /// A dormant or running application.
    App { app_id: AppId },
    /// A window collapsed into the field — explicit minimisation made visible. `identifier` is
    /// the foreign-toplevel identifier the compositor relays, which is also what a preview is
    /// filed under; `app_id` is its canonical owner when known.
    ParkedSession {
        app_id: Option<AppId>,
        identifier: String,
    },
    /// A non-destructive collection of references (`Tray`), never a filesystem folder — the
    /// shortcut pane: named, ordered, drawn from any number of directories, and adding to it
    /// never relocates the referenced item.
    Tray { tray_id: TrayId },
    /// One real directory, opened or expanded into its live contents. The placeable Home
    /// folder object: it binds exactly this path and nothing else.
    Folder {
        #[serde(with = "templar_types::native_path")]
        path: PathBuf,
    },
    /// The independent XDG Desktop staging shelf. Activity never grows or gardens it.
    /// `order` contains canonical base64 native paths, not lossy display names.
    DesktopDock {
        #[serde(with = "templar_types::native_path")]
        path: PathBuf,
        #[serde(default)]
        order: Vec<String>,
    },
    /// A live, data-first instrument (calendar, drives, inbox), optionally scoped to a project.
    Widget {
        widget: String,
        scope: Option<String>,
    },
    /// One file, by path. A reference: the file stays where it is.
    File {
        #[serde(with = "templar_types::native_path")]
        path: PathBuf,
    },
    /// A restrained, monochrome system control or measurement.
    System { probe: String },
}

/// A tray's identity (`crate::field::Tray`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TrayId(pub u64);

impl TileKind {
    /// Whether this kind is a container that may hold other tiles.
    pub fn is_container(&self) -> bool {
        matches!(
            self,
            Self::Project { .. } | Self::WindowGroup { .. } | Self::AppGroup { .. }
        )
    }

    /// Short lower-case name for text rendering and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Project { .. } => "project",
            Self::WindowGroup { .. } => "group",
            Self::AppGroup { .. } => "app group",
            Self::App { .. } => "app",
            Self::ParkedSession { .. } => "session",
            Self::Tray { .. } => "tray",
            Self::Folder { .. } => "folder",
            Self::DesktopDock { .. } => "desktop dock",
            Self::Widget { .. } => "widget",
            Self::File { .. } => "file",
            Self::System { .. } => "system",
        }
    }
}

/// How many grid cells a tile covers. Never zero in either axis.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub cols: u16,
    pub rows: u16,
}

impl Span {
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: if cols == 0 { 1 } else { cols },
            rows: if rows == 0 { 1 } else { rows },
        }
    }

    pub fn cells(self) -> u32 {
        u32::from(self.cols) * u32::from(self.rows)
    }

    /// Component-wise clamp into `[min, max]`.
    #[must_use]
    pub fn clamp(self, min: Span, max: Span) -> Span {
        Span::new(
            self.cols.clamp(min.cols, max.cols.max(min.cols)),
            self.rows.clamp(min.rows, max.rows.max(min.rows)),
        )
    }
}

/// The sizes a tile accepts: it is never laid out smaller than `min` or larger than `max`, and
/// asks for `preferred` when there is room.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpanRange {
    pub min: Span,
    pub preferred: Span,
    pub max: Span,
}

impl SpanRange {
    pub const fn fixed(span: Span) -> Self {
        Self {
            min: span,
            preferred: span,
            max: span,
        }
    }

    pub const fn new(min: Span, preferred: Span, max: Span) -> Self {
        Self {
            min,
            preferred,
            max,
        }
    }

    /// The default range for a kind, on a field `cols` wide and `rows` tall. These are the Home
    /// Field class floors, not generic layout guesses: a file is one thing, a folder is many,
    /// and a widget is an instrument. A project's preferred size remains one quadrant.
    pub fn for_kind(kind: &TileKind, cols: u16, rows: u16) -> Self {
        match kind {
            TileKind::Project { .. } => Self::new(
                Span::new(4, 4),
                Span::new((cols / 2).max(4), (rows / 2).max(4)),
                Span::new(cols, rows),
            ),
            // A window group is a container, but not a project-sized one. Its members are session
            // tiles, so it wants the room a couple of them need and no more — and its floor is
            // one cell, because the ladder's lowest rung for a group is a count badge and a tile
            // that cannot shrink to one could never show it. The interior grid is derived from
            // `preferred` (`ops::container_for`), not from this floor, so a group squeezed down
            // to a badge still holds everything it held.
            TileKind::WindowGroup { .. } => Self::new(
                Span::new(1, 1),
                Span::new(cols.clamp(1, 6), rows.clamp(1, 3)),
                Span::new(cols, rows),
            ),
            // A handful of favourites: a 2×2 preferred size shows four icons, and the interior
            // grid `ops::container_for` derives from it (4×4) holds sixteen, whatever the tile's
            // own span is squeezed to.
            TileKind::AppGroup { .. } => {
                Self::new(Span::new(2, 2), Span::new(2, 2), Span::new(6, 4))
            }
            TileKind::App { .. } => Self::new(Span::new(1, 1), Span::new(1, 1), Span::new(2, 2)),
            TileKind::File { .. } => Self::new(Span::new(1, 1), Span::new(1, 1), Span::new(2, 1)),
            TileKind::ParkedSession { .. } => {
                Self::new(Span::new(2, 1), Span::new(4, 2), Span::new(8, 4))
            }
            TileKind::Tray { .. } => Self::new(Span::new(2, 2), Span::new(3, 3), Span::new(6, 4)),
            // A folder is many things behind one door: the same class as a tray, with a larger
            // ceiling because a real directory can earn a treemap. Its floor is one row rather
            // than two, because a lightly used garden Place is a *line* — a name and a state
            // line — and the Folder rung floors already keep that at Small, where a line is all
            // that is drawn.
            TileKind::Folder { .. } => Self::new(Span::new(2, 1), Span::new(3, 3), Span::new(8, 6)),
            TileKind::DesktopDock { .. } => Self::new(
                Span::new(2, 1),
                Span::new(6, 2),
                Span::new(cols.max(6), rows.max(2)),
            ),
            // A widget is an instrument, and some of them (a launcher, a notification list) are
            // only useful large — so the ceiling is well above the preferred size rather than
            // equal to it, which is what used to make widgets the one kind that could shrink but
            // never grow. It stays a fixed, generous cap rather than scaling with the field:
            // nothing in the widget system may approach filling the screen, because a tile that
            // covers everything is a maximised window wearing a tile's clothes.
            TileKind::Widget { .. } => Self::new(Span::new(2, 2), Span::new(4, 4), Span::new(8, 6)),
            TileKind::System { .. } => Self::new(Span::new(3, 2), Span::new(4, 2), Span::new(7, 4)),
        }
    }

    /// Whether `span` is acceptable for this tile.
    pub fn accepts(&self, span: Span) -> bool {
        span.cols >= self.min.cols
            && span.rows >= self.min.rows
            && span.cols <= self.max.cols
            && span.rows <= self.max.rows
    }
}

/// One tile in the document. Where it is lives in the document, not here (`Located`), so a tile
/// on the shelf is simply one with no location.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tile {
    pub id: TileId,
    pub kind: TileKind,
    pub title: String,
    pub spans: SpanRange,
    /// A user-chosen or kind-derived accent (`0xRRGGBB`), or none for the kind's default.
    pub accent: Option<u32>,
}

/// How much of itself a tile shows. Presentation adapts to the allotted area: a tile that
/// becomes too small for one rung chooses a simpler one rather than shrinking its text
/// (`@templaros.decision.home-field-stable-bounded-semantic-grammar`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Representation {
    /// Icon or single figure only (1×1 for most kinds).
    Micro,
    /// Name and one line of state.
    Small,
    /// A row: name plus the next thing (a wide calendar, a session's title and count).
    Wide,
    /// The full tile form: preview, contents, agenda.
    Large,
    /// A container expanded to the whole field.
    Full,
}

/// The smallest span that admits a rung: a width, a height, and an area, all of which must be
/// met. Shape, not area alone — a 6×1 sliver has the cells of a 3×2 block and room for none of
/// the same content, and the old area-only ladder had no way to say so.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RungFloor {
    pub cols: u16,
    pub rows: u16,
    pub cells: u32,
}

impl RungFloor {
    /// A floor every span meets — for a rung a kind starts at rather than climbs to.
    pub const ANY: Self = Self {
        cols: 1,
        rows: 1,
        cells: 1,
    };

    pub const fn new(cols: u16, rows: u16, cells: u32) -> Self {
        Self { cols, rows, cells }
    }

    /// Whether `span` reaches this floor. A conjunction of `>=` in every axis, which is what
    /// makes [`Representation::for_span`] monotone by construction: growing a span can only turn
    /// a `false` here into a `true`, never the other way round.
    pub fn admits(self, span: Span) -> bool {
        span.cols >= self.cols && span.rows >= self.rows && span.cells() >= self.cells
    }

    /// Componentwise `<=`, the ordering a well-formed ladder's floors obey.
    pub fn at_most(self, other: Self) -> bool {
        self.cols <= other.cols && self.rows <= other.rows && self.cells <= other.cells
    }
}

/// The rungs one kind of tile climbs, as declared floors.
///
/// `base` is what the kind shows below every floor — the rung it is never smaller than. A `None`
/// floor means the kind never reaches that rung; `None`s only ever form a suffix, so the ladder
/// stays ordered. Two rungs may share a floor, which is how a kind *skips* one: a file tile's
/// `small` and `wide` are the same span, so growing a 1×1 file past one cell lands it on Wide
/// without a Small in between.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RungLadder {
    pub base: Representation,
    pub small: Option<RungFloor>,
    pub wide: Option<RungFloor>,
    pub large: Option<RungFloor>,
}

impl RungLadder {
    /// The floors, lowest rung first, paired with the rung each admits.
    pub fn steps(self) -> [(Representation, Option<RungFloor>); 3] {
        [
            (Representation::Small, self.small),
            (Representation::Wide, self.wide),
            (Representation::Large, self.large),
        ]
    }

    /// The rung `span` earns: the highest declared floor it reaches, never below `base`.
    pub fn rung_for(self, span: Span) -> Representation {
        let mut rung = self.base;
        for (candidate, floor) in self.steps() {
            if floor.is_some_and(|floor| floor.admits(span)) && candidate > rung {
                rung = candidate;
            }
        }
        rung
    }

    /// The ladder for a kind. These are declared shapes, not derived ones: the floors say what a
    /// rung's content *needs*, so the same numbers can be read next to the drawing code.
    pub fn for_kind(kind: &TileKind) -> Self {
        match kind {
            // A project is never smaller than 4×4, so its floors are its own minimum and the
            // point at which a quadrant becomes a full pane. Area does the work here; a project
            // cannot be a sliver.
            TileKind::Project { .. } => Self {
                base: Representation::Small,
                small: Some(RungFloor::ANY),
                wide: Some(RungFloor::new(4, 4, 16)),
                large: Some(RungFloor::new(4, 4, 36)),
            },
            // A window group's five rungs are the plan's own list: a count badge, then the name
            // and how many windows, then the name with the member titles in a row, then their
            // previews. Full is the zoom, which no span earns — a container reaches it by being
            // opened, exactly as a project does.
            //
            // Wide asks for three columns and Large for two rows, because a row of member titles
            // needs width and a row of previews needs both. A 2×1 group is still a name and a
            // count, which is the smallest thing worth saying about a set of windows.
            TileKind::WindowGroup { .. } => Self {
                base: Representation::Micro,
                small: Some(RungFloor::new(2, 1, 2)),
                wide: Some(RungFloor::new(3, 1, 3)),
                large: Some(RungFloor::new(4, 2, 8)),
            },
            // Containers and instruments: Wide is a row and Large is a body, and neither is a
            // line of text. Both floors demand two cells in *each* axis, which is the whole
            // point of the shape ladder — an 8×1 strip has the area of a 4×2 block and room for
            // none of the same content, so it stays at Small rather than being handed a body
            // that has to be truncated away again.
            TileKind::Tray { .. }
            | TileKind::Folder { .. }
            | TileKind::DesktopDock { .. }
            | TileKind::Widget { .. }
            | TileKind::AppGroup { .. } => Self {
                base: Representation::Small,
                small: Some(RungFloor::ANY),
                wide: Some(RungFloor::new(2, 2, 6)),
                large: Some(RungFloor::new(2, 2, 9)),
            },
            // A parked window is a title and a preview: it starts at Wide because there is
            // nothing smaller worth showing, and earns the preview at two rows.
            TileKind::ParkedSession { .. } => Self {
                base: Representation::Wide,
                small: Some(RungFloor::ANY),
                wide: Some(RungFloor::ANY),
                large: Some(RungFloor::new(2, 2, 4)),
            },
            // An application is an icon, and a name once there is a second cell to put it in.
            // It never climbs further: the ceiling is 2×2.
            TileKind::App { .. } => Self {
                base: Representation::Micro,
                small: Some(RungFloor::new(1, 1, 2)),
                wide: None,
                large: None,
            },
            // A file skips Small: one cell is the icon, two is the name beside it, and the
            // ceiling is 2×1 so there is no third form.
            TileKind::File { .. } => Self {
                base: Representation::Micro,
                small: Some(RungFloor::new(1, 1, 2)),
                wide: Some(RungFloor::new(1, 1, 2)),
                large: None,
            },
            // Three rungs, not two: a system tile's controls are declared per rung
            // (`templar-shell`'s widget contract), and jumping straight from its 3×2 floor to
            // everything-at-once left the middle sizes with nothing to say.
            //
            // The rows floor stays at 2 all the way up on purpose. A system widget's controls
            // lay out in a row, so a 7×2 Settings tile is a Large — demanding height for the
            // top rung would take the controls off the one shape the shelf actually places.
            TileKind::System { .. } => Self {
                base: Representation::Small,
                small: Some(RungFloor::ANY),
                wide: Some(RungFloor::new(3, 2, 7)),
                large: Some(RungFloor::new(4, 2, 13)),
            },
        }
    }
}

impl Representation {
    /// The rung a tile of `kind` occupies at `span`. Monotone: growing a span never lowers the
    /// rung (property-tested), which holds by construction — see [`RungFloor::admits`].
    pub fn for_span(kind: &TileKind, span: Span) -> Self {
        RungLadder::for_kind(kind).rung_for(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// One of every kind, for the ladder properties. The payloads are irrelevant — the ladder is
    /// declared per kind, never per instance.
    fn every_kind() -> Vec<TileKind> {
        vec![
            TileKind::Project {
                project_id: "p".into(),
            },
            TileKind::App {
                app_id: "org.templaros.Files".parse().unwrap(),
            },
            TileKind::ParkedSession {
                app_id: None,
                identifier: "window:1".into(),
            },
            TileKind::Tray { tray_id: TrayId(1) },
            TileKind::Folder { path: "x".into() },
            TileKind::DesktopDock {
                path: "x".into(),
                order: Vec::new(),
            },
            TileKind::Widget {
                widget: "calendar".into(),
                scope: None,
            },
            TileKind::File { path: "x".into() },
            TileKind::System {
                probe: "drives".into(),
            },
            TileKind::WindowGroup {
                identifier: "4".into(),
            },
            TileKind::AppGroup {
                group_id: "g".into(),
            },
        ]
    }

    /// The kinds that existed while the ladder was area-only, and so have something for the
    /// shape ladder to be compared against. `WindowGroup` arrived after it and never had an area
    /// ladder to agree with — comparing it against one written here would be comparing the shape
    /// ladder with a transcription of itself.
    fn kinds_with_an_area_ladder() -> Vec<TileKind> {
        every_kind()
            .into_iter()
            .filter(|kind| {
                !matches!(
                    kind,
                    TileKind::WindowGroup { .. } | TileKind::AppGroup { .. }
                )
            })
            .collect()
    }

    #[test]
    fn spans_are_never_zero() {
        assert_eq!(Span::new(0, 0), Span::new(1, 1));
        assert_eq!(Span::new(3, 0).rows, 1);
    }

    #[test]
    fn a_project_prefers_one_quadrant() {
        let kind = TileKind::Project {
            project_id: "p".into(),
        };
        let range = SpanRange::for_kind(&kind, 12, 8);
        assert_eq!(range.preferred, Span::new(6, 4));
        assert_eq!(range.max, Span::new(12, 8));
        assert!(range.accepts(Span::new(4, 4)));
        assert!(!range.accepts(Span::new(3, 4)));
    }

    #[test]
    fn object_class_size_floors_match_the_home_field_language() {
        let field = (24, 13);
        let cases = [
            (
                TileKind::File { path: "x".into() },
                Span::new(1, 1),
                Span::new(2, 1),
            ),
            (
                TileKind::Tray { tray_id: TrayId(1) },
                Span::new(2, 2),
                Span::new(6, 4),
            ),
            (
                TileKind::Widget {
                    widget: "calendar".into(),
                    scope: None,
                },
                Span::new(2, 2),
                Span::new(8, 6),
            ),
            (
                TileKind::App {
                    app_id: "org.templaros.Files".parse().unwrap(),
                },
                Span::new(1, 1),
                Span::new(2, 2),
            ),
            (
                TileKind::ParkedSession {
                    app_id: None,
                    identifier: "window:1".into(),
                },
                Span::new(2, 1),
                Span::new(8, 4),
            ),
            (
                TileKind::Project {
                    project_id: "p".into(),
                },
                Span::new(4, 4),
                Span::new(24, 13),
            ),
            (
                TileKind::System {
                    probe: "drives".into(),
                },
                Span::new(3, 2),
                Span::new(7, 4),
            ),
        ];
        for (kind, min, max) in cases {
            let range = SpanRange::for_kind(&kind, field.0, field.1);
            assert_eq!(range.min, min, "minimum for {kind:?}");
            assert_eq!(range.max, max, "maximum for {kind:?}");
        }
    }

    #[test]
    fn growing_a_span_never_lowers_the_rung() {
        let kinds = [
            TileKind::Project {
                project_id: "p".into(),
            },
            TileKind::Tray { tray_id: TrayId(1) },
            TileKind::Widget {
                widget: "calendar".into(),
                scope: None,
            },
            TileKind::ParkedSession {
                app_id: None,
                identifier: "x".into(),
            },
            TileKind::System {
                probe: "drives".into(),
            },
        ];
        for kind in &kinds {
            for cols in 1..=6u16 {
                for rows in 1..=6u16 {
                    let here = Representation::for_span(kind, Span::new(cols, rows));
                    let wider = Representation::for_span(kind, Span::new(cols + 1, rows));
                    let taller = Representation::for_span(kind, Span::new(cols, rows + 1));
                    assert!(
                        wider >= here,
                        "{kind:?} {cols}x{rows} → wider lowered the rung"
                    );
                    assert!(
                        taller >= here,
                        "{kind:?} {cols}x{rows} → taller lowered the rung"
                    );
                }
            }
        }
    }

    /// A ladder is only meaningful if its floors are ordered: a rung may never be cheaper than
    /// the one below it, and a kind that stops climbing stops for good — the gaps are a suffix,
    /// never a hole in the middle.
    #[test]
    fn every_ladder_is_ordered_and_its_gaps_form_a_suffix() {
        for kind in every_kind() {
            let ladder = RungLadder::for_kind(&kind);
            let mut previous: Option<RungFloor> = None;
            let mut stopped = false;
            for (rung, floor) in ladder.steps() {
                match floor {
                    None => stopped = true,
                    Some(floor) => {
                        assert!(!stopped, "{kind:?}: {rung:?} sits above an undeclared rung");
                        if let Some(previous) = previous {
                            assert!(
                                previous.at_most(floor),
                                "{kind:?}: {rung:?} floor {floor:?} is below {previous:?}"
                            );
                        }
                        previous = Some(floor);
                    }
                }
                // A rung at or below the base is one the kind is never smaller than, so the only
                // floor it may carry is the one every span meets. Anything else would read as a
                // threshold that does nothing.
                if rung <= ladder.base {
                    assert_eq!(
                        floor,
                        Some(RungFloor::ANY),
                        "{kind:?}: {rung:?} is at or below the base rung {:?} but declares a floor",
                        ladder.base
                    );
                }
            }
        }
    }

    /// Every rung a kind declares must be one a tile of that kind can actually be given. A floor
    /// above the class ceiling is dead drawing code that nobody will ever see fail.
    #[test]
    fn every_declared_floor_fits_inside_the_kinds_ceiling() {
        for kind in every_kind() {
            let range = SpanRange::for_kind(&kind, 24, 13);
            let ceiling = range.max;
            for (rung, floor) in RungLadder::for_kind(&kind).steps() {
                let Some(floor) = floor else { continue };
                assert!(
                    floor.cols <= ceiling.cols
                        && floor.rows <= ceiling.rows
                        && floor.cells <= ceiling.cells(),
                    "{kind:?}: {rung:?} floor {floor:?} is above the {ceiling:?} ceiling"
                );
                assert!(
                    floor.admits(Span::new(ceiling.cols, ceiling.rows)),
                    "{kind:?}: {rung:?} floor {floor:?} is unreachable at the ceiling"
                );
            }
            // And the base is what the floor of the class shows.
            assert!(
                Representation::for_span(&kind, range.min) >= RungLadder::for_kind(&kind).base,
                "{kind:?}: the class minimum falls below the base rung"
            );
        }
    }

    /// The ladder the old area-only `match span.cells()` expressed, kept here as the reference
    /// the shape ladder is compared against.
    fn rung_by_area_alone(kind: &TileKind, span: Span) -> Representation {
        let cells = span.cells();
        match kind {
            TileKind::Project { .. } => match cells {
                0..=15 => Representation::Small,
                16..=35 => Representation::Wide,
                _ => Representation::Large,
            },
            TileKind::Tray { .. }
            | TileKind::Folder { .. }
            | TileKind::DesktopDock { .. }
            | TileKind::Widget { .. } => match cells {
                0..=4 => Representation::Small,
                5..=8 => Representation::Wide,
                _ => Representation::Large,
            },
            TileKind::ParkedSession { .. } => match cells {
                0..=3 => Representation::Wide,
                _ => Representation::Large,
            },
            TileKind::App { .. } => {
                if cells == 1 {
                    Representation::Micro
                } else {
                    Representation::Small
                }
            }
            TileKind::File { .. } => {
                if cells == 1 {
                    Representation::Micro
                } else {
                    Representation::Wide
                }
            }
            TileKind::System { .. } => match cells {
                0..=6 => Representation::Small,
                7..=12 => Representation::Wide,
                _ => Representation::Large,
            },
            // Never had one; `kinds_with_an_area_ladder` is what keeps this unreachable, and the
            // panic is what makes that filter's removal visible instead of silent.
            TileKind::WindowGroup { .. } | TileKind::AppGroup { .. } => {
                unreachable!("a window group predates no area ladder — it postdates the shape one")
            }
        }
    }

    /// On every shape a tile is ordinarily given — inside its class range, and no more distorted
    /// than 1:3 — the shape ladder says exactly what the area ladder said. The change is confined
    /// to slivers, which is the only place area was lying.
    #[test]
    fn the_shape_ladder_agrees_with_the_area_ladder_on_every_ordinary_shape() {
        for kind in kinds_with_an_area_ladder() {
            let range = SpanRange::for_kind(&kind, 24, 13);
            for cols in 1..=24u16 {
                for rows in 1..=13u16 {
                    let span = Span::new(cols, rows);
                    if !range.accepts(span) {
                        continue;
                    }
                    let (long, short) = (cols.max(rows), cols.min(rows));
                    if u32::from(long) > 3 * u32::from(short) {
                        continue;
                    }
                    assert_eq!(
                        Representation::for_span(&kind, span),
                        rung_by_area_alone(&kind, span),
                        "{kind:?} at {cols}x{rows}"
                    );
                }
            }
        }
    }

    /// And where it disagrees, it disagrees downwards, for the reason the ladder exists: a strip
    /// one cell tall has no room for the body its area would have bought it.
    #[test]
    fn a_sliver_settles_for_a_lower_rung_than_its_area_would_buy() {
        let widget = TileKind::Widget {
            widget: "calendar".into(),
            scope: None,
        };
        let session = TileKind::ParkedSession {
            app_id: None,
            identifier: "window:1".into(),
        };
        let cases = [
            (&widget, Span::new(6, 1), Representation::Small),
            (&widget, Span::new(8, 1), Representation::Small),
            (&widget, Span::new(1, 6), Representation::Small),
            (&session, Span::new(4, 1), Representation::Wide),
            (&session, Span::new(8, 1), Representation::Wide),
        ];
        for (kind, span, expected) in cases {
            let now = Representation::for_span(kind, span);
            assert_eq!(now, expected, "{kind:?} at {span:?}");
            assert!(
                now < rung_by_area_alone(kind, span),
                "{kind:?} at {span:?} was supposed to step down from the area ladder"
            );
        }
    }

    proptest! {
        /// Monotonicity, over the whole grid rather than the 6×6 corner the hand-written test
        /// covers: growing a tile in either axis never takes content away from it. This is what
        /// keeps a resize from flickering between two forms, and it holds by construction —
        /// every floor is a conjunction of `>=`.
        #[test]
        fn growing_a_span_in_either_axis_never_lowers_the_rung(
            kind_index in 0usize..10,
            cols in 1u16..64,
            rows in 1u16..64,
        ) {
            let kinds = every_kind();
            let kind = &kinds[kind_index];
            let here = Representation::for_span(kind, Span::new(cols, rows));
            prop_assert!(Representation::for_span(kind, Span::new(cols + 1, rows)) >= here);
            prop_assert!(Representation::for_span(kind, Span::new(cols, rows + 1)) >= here);
        }
    }
}
