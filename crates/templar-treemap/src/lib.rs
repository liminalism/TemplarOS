//! Cushion treemaps (van Wijk & van de Wetering, INFOVIS 1999) for the Home Field's file trays:
//! the *interior* of a tray or a standard-path drawer tile, where visual area proportional to
//! bytes is the right semantics — never the field's own layout, which optimises stability instead
//! (`@templaros.decision.home-field-stable-bounded-semantic-grammar`).
//!
//! Lifted from Silvaview-rs (the author's SequoiaView revival, AGPL-3.0-only like this
//! workspace, ADR-0013), keeping exactly its pure parts: the arena [`tree`], the squarified
//! [`layout`] with level-of-detail culling and chain collapse, the category [`colors`] with the
//! pastel↔bold axis, and the CPU [`cushion`] rasterizer. Its GPU path, Vello scene, NTFS scanner
//! and UI stayed behind. A bounded std-only [`scan`] replaces the parallel walker: a tray shows a
//! directory, not a disk.
//!
//! The algorithm's source: J. J. van Wijk and H. van de Wetering, *Cushion Treemaps:
//! Visualization of Hierarchical Information*, Proc. INFOVIS 1999; the squarified layout is
//! Bruls, Huizing and van Wijk, *Squarified Treemaps*, 2000.
//!
//! [`garden`] holds the Homegarden Place layouts: the shape of a Place's relevance-weighted
//! used-structure tree as stable treemaps, Voronoi and circle packings, radial partitions,
//! grown branches, fractals and folios, all ordered by stable node id rather than weight.

#![forbid(unsafe_code)]
// Lifted numeric code: many short names and wide signatures are the paper's own.
#![allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::needless_range_loop,
    clippy::cast_lossless
)]

pub mod colors;
pub mod cushion;
pub mod garden;
pub mod layout;
pub mod scan;
pub mod tree;

pub use colors::{AppColor, ColorMode, ColorSettings};
pub use cushion::{CushionConfig, rasterize_argb, rasterize_cushions};
pub use layout::{Layout, LayoutConfig, LayoutRect, compute_layout_in_rect};
pub use scan::{RawEntry, ScanLimits, ScanOutcome, scan};
pub use tree::arena::{FileNode, FileTree, NodeId};
pub use tree::build_tree;
pub use tree::extensions::{CATEGORY_COUNT, FileCategory, categorize_extension, classify_path};
