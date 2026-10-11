//! The Home Field model (`@templaros.decision.home-field-stable-bounded-semantic-grammar`): a bounded grid of rectangular tiles — projects,
//! applications and parked sessions, file trays, widgets, system tiles — that is the desktop
//! surface of TemplarOS. No wallpaper, no icons, no taskbar: the field is a materialised model of
//! current work, and windows collapse into it and expand back out of it.
//!
//! This crate is the *model* and nothing else. It owns:
//!
//! * the document ([`FieldDocument`]): tiles, where each one is (top level or inside one
//!   project container, one level deep, or on the shelf with no place at all), trays of
//!   file references, and the grid's size;
//! * every operation on it ([`FieldOp`]), each of which is applied by [`apply`] and returns its
//!   exact inverse — which is the whole undo design: undo is `apply(inverse)`;
//! * the stable local layout ([`reflow`]): inserting a tile disturbs only the rows, columns or
//!   container it lands in, and a cost that penalises moving anything else far above everything
//!   else decides between candidates;
//! * the representation ladder ([`Representation`]): how much a tile shows at a given size;
//! * the small semantic grammar of drops ([`grammar`]): source kind × target → one named,
//!   previewable operation, or plain placement;
//! * the drag/throw state machine ([`drag`]), which derives velocity itself so the toolkit
//!   need not;
//! * keyboard navigation ([`focus`]) and the text rendering ([`text`]) that keeps the field
//!   readable without a display.
//!
//! It has no async runtime, no D-Bus and no toolkit dependency, so the shell and sessiond share
//! it without either dragging the other's world in, and every invariant is property-tested here
//! (`tests/field.rs`) rather than discovered in a running session.
//!
//! Two layout systems, deliberately (`@templaros.decision.home-field-stable-bounded-semantic-grammar`):
//! the field's own grid optimises *stability* — nothing moves unless the user moved it — while a
//! tray's interior may use a byte-proportional treemap. This crate is only the former.

#![forbid(unsafe_code)]

pub mod desktop_dock;
pub mod drag;
pub mod field;
pub mod focus;
pub mod garden;
pub mod grammar;
pub mod migrate;
pub mod ops;
pub mod panel;
pub mod reflow;
pub mod selection;
pub mod text;
pub mod tile;

pub use desktop_dock::ensure_desktop_dock;
pub use drag::{Drag, DragConfig, DragSample};
pub use field::{
    Cell, Container, FieldDocument, Landing, Located, PixelRect, Placement, Tray, cell_at,
    cell_rect, tile_rect,
};
pub use grammar::{DropModifiers, DropOrigin, Operation, Target, operation_for};
pub use migrate::{DOCUMENT_MIGRATIONS, DocumentMigration, LoadError, load_document};
pub use ops::{FieldError, FieldOp, apply, folder_tile, place_folder_op, reshape};
pub use panel::{Arrangement, Edge, Orientation, Panel, PanelId, arrangement_layout, panel_id_for};
pub use reflow::{LayoutCost, Reflow};
pub use selection::Selection;
pub use tile::{
    Representation, RungFloor, RungLadder, Span, SpanRange, Tile, TileId, TileKind, TrayId,
};

/// The document format version this crate writes.
///
/// [`ops::apply`] requires an in-memory document to be exactly this version — applying an
/// operation to a shape this code does not understand is how a layout gets silently mangled. At
/// the *load* boundary the two directions differ: an older document is brought forward through
/// [`migrate::DOCUMENT_MIGRATIONS`], and a newer one is refused with the stored copy left
/// untouched. See [`migrate`].
pub const DOCUMENT_VERSION: u32 = 3;
