//! Tiles: what one is, how big it may be, and how much it shows at a given size.

use lege_types::AppId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A tile's identity, stable for the document's lifetime — never reused.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TileId(pub u64);

/// What a tile stands for. The outer shape is always a rectangle; type is expressed through
/// internal structure and behaviour (`docs/homefield.md` "Visual identity"), so this is the one
/// place the kinds are enumerated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TileKind {
    /// A container: files, tools, sessions and scoped widgets that belong together. Refers to a
    /// `Projects1` project; holds other tiles one level deep.
    Project { project_id: String },
    /// A dormant or running application.
    App { app_id: AppId },
    /// A window collapsed into the field — explicit minimisation made visible. `identifier` is
    /// the foreign-toplevel identifier the compositor relays, which is also what a preview is
    /// filed under; `app_id` is its canonical owner when known.
    ParkedSession {
        app_id: Option<AppId>,
        identifier: String,
    },
    /// A non-destructive collection of references (`Tray`), never a filesystem folder.
    Tray { tray_id: TrayId },
    /// A live, data-first instrument (calendar, drives, inbox), optionally scoped to a project.
    Widget {
        widget: String,
        scope: Option<String>,
    },
    /// One file, by path. A reference: the file stays where it is.
    File { path: PathBuf },
    /// A restrained, monochrome system control or measurement.
    System { probe: String },
}

/// A tray's identity (`crate::field::Tray`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TrayId(pub u64);

impl TileKind {
    /// Whether this kind is a container that may hold other tiles.
    pub fn is_container(&self) -> bool {
        matches!(self, Self::Project { .. })
    }

    /// Short lower-case name for text rendering and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Project { .. } => "project",
            Self::App { .. } => "app",
            Self::ParkedSession { .. } => "session",
            Self::Tray { .. } => "tray",
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
            TileKind::App { .. } => Self::new(Span::new(1, 1), Span::new(1, 1), Span::new(2, 2)),
            TileKind::File { .. } => Self::new(Span::new(1, 1), Span::new(1, 1), Span::new(2, 1)),
            TileKind::ParkedSession { .. } => {
                Self::new(Span::new(2, 1), Span::new(4, 2), Span::new(8, 4))
            }
            TileKind::Tray { .. } => Self::new(Span::new(2, 2), Span::new(3, 3), Span::new(6, 4)),
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
/// (`docs/homefield.md` "What malleable should mean").
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

impl Representation {
    /// The rung a tile of `kind` occupies at `span`. Monotone: growing a span never lowers the
    /// rung (property-tested).
    pub fn for_span(kind: &TileKind, span: Span) -> Self {
        let cells = span.cells();
        match kind {
            TileKind::Project { .. } => match cells {
                0..=15 => Self::Small,
                16..=35 => Self::Wide,
                _ => Self::Large,
            },
            TileKind::Tray { .. } | TileKind::Widget { .. } => {
                if cells <= 4 {
                    Self::Small
                } else if cells <= 8 {
                    Self::Wide
                } else {
                    Self::Large
                }
            }
            TileKind::ParkedSession { .. } => match cells {
                0..=3 => Self::Wide,
                _ => Self::Large,
            },
            TileKind::App { .. } => {
                if cells == 1 {
                    Self::Micro
                } else {
                    Self::Small
                }
            }
            TileKind::File { .. } => {
                if cells == 1 {
                    Self::Micro
                } else {
                    Self::Wide
                }
            }
            TileKind::System { .. } => {
                // Three rungs, not two: a system tile's controls are declared per rung
                // (`lege-shell`'s widget contract), and jumping straight from its 3x2 floor to
                // everything-at-once left the middle sizes with nothing to say.
                if cells <= 6 {
                    Self::Small
                } else if cells <= 12 {
                    Self::Wide
                } else {
                    Self::Large
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    app_id: "org.legeos.Files".parse().unwrap(),
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
}
