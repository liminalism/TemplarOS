//! The forms a Place draws in, in the style the theme chooses
//! (`@templaros.decision.a-place-is-a-drawn-structure-not-a-richer-rectangle`).
//!
//! The field's layout engine keeps allocating rectangles; what is drawn inside one is not a
//! card. The ledger settles which *form* a place is in — `branch` for a place worked through,
//! `shelf` for one looked at, `folio` for a working set of documents, `plain` for one that has
//! earned none — and holds it for two sessions, so nothing here decides identity from whatever
//! happens to be in the tree this frame. This module decides only how that form is *drawn*:
//!
//! - [`themes`] maps each form to a structure (`templar_treemap::garden::LayoutKind`) and a
//!   material, per `shell.home-garden.theme`;
//! - the used-structure tree is cut to what the tile has room for ([`Budget`],
//!   [`weighted_tree`]) and laid out by `templar_treemap::garden`, which keeps siblings where
//!   they were as their weights drift — position is memory;
//! - `paint` rasterizes the shapes once into a bitmap, which `cache` holds until the tree, the
//!   size, the scale or the theme changes;
//! - the names, the hover and the bloom marks are drawn live over it every frame, so they are
//!   crisp, coloured by state, and clickable exactly as the rows of a folder are.
//!
//! The grammar the drawing keeps: shape is the mode, material is identity (grain seeded by the
//! place's `material_seed`, never a hue), colour is state and scarce (the accent marks what
//! arrived and what is under the pointer, nothing else), size is relevance.

mod cache;
mod paint;
pub mod themes;

use std::path::Path;

use pixelkit_raster::{Rect, ScaleFilter};
use pixelkit_text::Align;
use pixelkit_ui::Ui;
use templar_field::TileId;
use templar_treemap::garden::{self as layouts, Bounds, LayoutRequest, WeightedNode};

use crate::garden::{GardenPlace, TreeNode};
use crate::surface::design;
use crate::surface::field_screen::FieldAction;

pub use themes::{DEFAULT_THEME, THEMES, Theme, theme_named};

/// How much of a place's tree a body of a given size can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub max_nodes: usize,
    pub max_depth: u8,
}

/// Logical square pixels one drawn node needs to be a legible shape rather than a speck.
const AREA_PER_NODE: f32 = 2500.0;

impl Budget {
    /// The budget of a `width`×`height` body in logical pixels: about one node per
    /// [`AREA_PER_NODE`], and two to five levels, deeper as the body grows.
    #[must_use]
    pub fn for_area(width: f32, height: f32) -> Self {
        let area = width.max(0.0) * height.max(0.0);
        let max_depth = 2 + [40_000.0, 90_000.0, 160_000.0]
            .iter()
            .filter(|&&floor| area >= floor)
            .count() as u8;
        Self {
            max_nodes: (area / AREA_PER_NODE) as usize,
            max_depth,
        }
    }
}

/// The served tree as the layout engine's input, cut to `budget`: nothing below
/// `budget.max_depth`, nothing without weight, and — past `budget.max_nodes` — the weakest
/// leaves dropped first, so what survives is the part of the place that was actually used.
///
/// Each node's id is `node_id` of its path relative to the place's root, which is what keeps it
/// in the same position when its weight moves.
#[must_use]
pub fn weighted_tree(tree: &[TreeNode], budget: Budget) -> Vec<WeightedNode> {
    fn convert(nodes: &[TreeNode], prefix: &str, depth: u8, budget: Budget) -> Vec<WeightedNode> {
        if depth >= budget.max_depth {
            return Vec::new();
        }
        nodes
            .iter()
            .filter(|node| node.score.is_finite() && node.score > 0.0 && !node.name.is_empty())
            .map(|node| {
                let relative = if prefix.is_empty() {
                    node.name.clone()
                } else {
                    format!("{prefix}/{}", node.name)
                };
                WeightedNode {
                    id: layouts::node_id(&relative),
                    name: node.name.clone(),
                    weight: node.score,
                    is_dir: node.is_dir,
                    children: convert(&node.children, &relative, depth + 1, budget),
                }
            })
            .collect()
    }
    fn count(nodes: &[WeightedNode]) -> usize {
        nodes.iter().map(|node| 1 + count(&node.children)).sum()
    }
    /// The weakest leaf: `(weight, id)` smallest, the id breaking ties so the cut is the same
    /// every time.
    fn weakest(nodes: &[WeightedNode]) -> Option<(f32, u64)> {
        nodes
            .iter()
            .filter_map(|node| {
                if node.children.is_empty() {
                    Some((node.weight, node.id))
                } else {
                    weakest(&node.children)
                }
            })
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
    }
    fn remove(nodes: &mut Vec<WeightedNode>, id: u64) -> bool {
        if let Some(index) = nodes
            .iter()
            .position(|node| node.id == id && node.children.is_empty())
        {
            nodes.remove(index);
            return true;
        }
        nodes.iter_mut().any(|node| remove(&mut node.children, id))
    }

    let mut roots = convert(tree, "", 0, budget);
    let mut total = count(&roots);
    while total > budget.max_nodes {
        let Some((_, id)) = weakest(&roots) else {
            break;
        };
        if !remove(&mut roots, id) {
            break;
        }
        total -= 1;
    }
    roots
}

/// A stable digest of a served tree and the budget it is cut to: equal digests lay out alike.
fn fingerprint(tree: &[TreeNode], budget: Budget) -> u64 {
    fn feed(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash = (*hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn walk(hash: &mut u64, nodes: &[TreeNode]) {
        feed(hash, &(nodes.len() as u64).to_le_bytes());
        for node in nodes {
            feed(hash, node.name.as_bytes());
            feed(hash, &[0, u8::from(node.is_dir)]);
            feed(hash, &node.score.to_bits().to_le_bytes());
            walk(hash, &node.children);
        }
    }
    let mut hash = 0xcbf2_9ce4_8422_2325;
    feed(&mut hash, &(budget.max_nodes as u64).to_le_bytes());
    feed(&mut hash, &[budget.max_depth]);
    walk(&mut hash, tree);
    hash
}

/// Each node's path relative to the root and its name, keyed by id, for resolving layout items.
fn index(nodes: &[WeightedNode], prefix: &str, into: &mut Vec<(u64, String, String)>) {
    for node in nodes {
        let relative = if prefix.is_empty() {
            node.name.clone()
        } else {
            format!("{prefix}/{}", node.name)
        };
        index(&node.children, &relative, into);
        into.push((node.id, relative, node.name.clone()));
    }
}

/// The drawing state of every Place on Home: the theme, and each tile's cached body.
#[derive(Debug)]
pub struct PlaceStyles {
    theme: &'static Theme,
    cache: cache::BodyCache,
}

impl Default for PlaceStyles {
    fn default() -> Self {
        Self {
            theme: theme_named(DEFAULT_THEME),
            cache: cache::BodyCache::default(),
        }
    }
}

/// The height of one name, logical px: the layouts size their label room for it.
const LABEL_LINE: i32 = 14;
/// The narrowest room a name is drawn in at all; below it a clipped word is worse than none.
const LABEL_MIN: i32 = 24;

impl PlaceStyles {
    /// Draw places in the theme called `name` (the default for a name this build lacks). The
    /// bodies drawn in the previous theme are rebuilt as they are next painted.
    pub fn set_theme(&mut self, name: &str) {
        self.theme = theme_named(name);
    }

    #[must_use]
    pub fn theme(&self) -> &'static Theme {
        self.theme
    }

    /// Seeded neutral paper for the new project object, cached independently of its contents.
    /// Directory topology is deliberately not an input to this material.
    pub fn paint_material(
        &mut self,
        ui: &mut Ui<'_>,
        rect: Rect,
        tile: TileId,
        place: &GardenPlace,
        fill: u32,
    ) {
        if rect.w <= 0 || rect.h <= 0 {
            return;
        }
        let Some(style) = self.theme.style_for("plain", true) else {
            return;
        };
        let scale = ui.scale.0;
        let theme = self.theme.name;
        let palette = design::palette();
        if let Some(body) = self.cache.matching(tile, |key| {
            key.form == "material"
                && key.root == place.root
                && key.tree == u64::from(fill)
                && key.size == (rect.w, rect.h)
                && key.scale == scale.to_bits()
                && key.theme == theme
                && key.palette == palette
                && key.seed == place.material_seed
        }) {
            ui.painter.blit(&body.bitmap, rect, ScaleFilter::Bilinear);
            return;
        }
        let key = cache::BodyKey {
            root: place.root.clone(),
            tree: u64::from(fill),
            size: (rect.w, rect.h),
            scale: scale.to_bits(),
            theme,
            palette,
            form: "material".to_owned(),
            seed: place.material_seed,
        };
        let body = self.cache.get_or_build(tile, key, |_| cache::Built {
            layout: layouts::layout(&LayoutRequest {
                kind: style.kind,
                roots: &[],
                bounds: Bounds {
                    x: 0.0,
                    y: 0.0,
                    w: rect.w as f32 / scale,
                    h: rect.h as f32 / scale,
                },
                seed: place.material_seed,
                max_depth: 0,
                max_nodes: 0,
                previous: None,
            }),
            paths: Vec::new(),
            names: Vec::new(),
            bitmap: paint::material(
                rect.w as u32,
                rect.h as u32,
                fill,
                &style.grain,
                place.material_seed,
                scale,
            ),
        });
        ui.painter.blit(&body.bitmap, rect, ScaleFilter::Bilinear);
    }

    /// Draw `place`'s body into `rect`, the part of its tile below the state line. Returns
    /// whether anything was drawn: `false` for a form the theme leaves plain at this rung, or a
    /// tree with nothing in it — the caller's to fill (a large plain place is browsable).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn paint(
        &mut self,
        ui: &mut Ui<'_>,
        rect: Rect,
        tile: TileId,
        root: &Path,
        place: &GardenPlace,
        large: bool,
        actions: &mut Vec<FieldAction>,
    ) -> bool {
        let Some(style) = self.theme.style_for(&place.form, large) else {
            return false;
        };
        if rect.w <= 0 || rect.h <= 0 || place.tree.is_empty() {
            return false;
        }
        let scale = ui.scale.0;
        let logical = (rect.w as f32 / scale, rect.h as f32 / scale);
        let budget = Budget::for_area(logical.0, logical.1);
        let key = cache::BodyKey {
            root: root.to_path_buf(),
            tree: fingerprint(&place.tree, budget),
            size: (rect.w, rect.h),
            scale: scale.to_bits(),
            theme: self.theme.name,
            palette: design::palette(),
            form: place.form.clone(),
            seed: place.material_seed,
        };
        let palette = key.palette;
        let kernel = &mut *ui.kernel;
        let body = self.cache.get_or_build(tile, key, |previous| {
            let roots = weighted_tree(&place.tree, budget);
            let layout = layouts::layout(&LayoutRequest {
                kind: style.kind,
                roots: &roots,
                bounds: Bounds {
                    x: 0.0,
                    y: 0.0,
                    w: logical.0,
                    h: logical.1,
                },
                seed: place.material_seed,
                max_depth: budget.max_depth,
                max_nodes: budget.max_nodes,
                previous,
            });
            let mut known = Vec::new();
            index(&roots, "", &mut known);
            known.sort_unstable_by_key(|(id, ..)| *id);
            let (paths, names) = layout
                .items
                .iter()
                .map(|item| {
                    known
                        .binary_search_by_key(&item.node, |(id, ..)| *id)
                        .map_or_else(
                            |_| (String::new(), String::new()),
                            |at| (known[at].1.clone(), known[at].2.clone()),
                        )
                })
                .unzip();
            let bitmap = paint::rasterize(
                &layout,
                style,
                &palette,
                rect.w as u32,
                rect.h as u32,
                scale,
                place.material_seed,
                kernel,
            );
            cache::Built {
                layout,
                paths,
                names,
                bitmap,
            }
        });
        if body.layout.items.is_empty() {
            return false;
        }
        ui.painter.blit(&body.bitmap, rect, ScaleFilter::Nearest);

        // What is under the pointer: the topmost shape there (items are in paint order), in
        // the body's own logical coordinates.
        let pointer = ui
            .input
            .cursor()
            .filter(|&(x, y)| rect.contains(x, y))
            .map(|(x, y)| {
                (
                    (x - rect.x) as f32 / scale + 0.5 / scale,
                    (y - rect.y) as f32 / scale + 0.5 / scale,
                )
            });
        let under = |index: usize| {
            pointer
                .is_some_and(|point| paint::contains(&body.layout.items[index].shape, point, 3.0))
        };
        let bloomed: Vec<&Path> = place
            .bloomed
            .iter()
            .filter_map(|path| path.strip_prefix(root).ok())
            .collect();
        let is_bloomed = |index: usize| {
            let relative = &body.paths[index];
            !relative.is_empty() && bloomed.iter().any(|path| *path == Path::new(relative))
        };

        // Names first, so hovering one wins over the shape it happens to sit on.
        let line = ui.px(LABEL_LINE);
        let style_text = design::state_style(ui);
        let mut placed: Vec<Rect> = Vec::new();
        let mut labels: Vec<(usize, Rect)> = Vec::new();
        for (index, item) in body.layout.items.iter().enumerate() {
            let Some((x, y, width)) = item.label else {
                continue;
            };
            let name = &body.names[index];
            if name.is_empty() {
                continue;
            }
            let space = ui
                .scale
                .pxf(width)
                .min(rect.right() - rect.x - ui.scale.pxf(x));
            if space < ui.px(LABEL_MIN) {
                continue;
            }
            let wanted = ui.text.measure(name, style_text) + ui.px(2);
            let label = Rect::new(
                rect.x + ui.scale.pxf(x),
                rect.y + ui.scale.pxf(y) - line / 2,
                wanted.min(space),
                line,
            )
            .intersect(rect);
            if label.w < ui.px(LABEL_MIN).min(wanted)
                || placed.iter().any(|other| overlaps(*other, label))
            {
                continue;
            }
            placed.push(label);
            labels.push((index, label));
        }
        let hovered = labels
            .iter()
            .find(|(_, label)| ui.input.hovering(*label))
            .map(|(index, _)| *index)
            .or_else(|| {
                (0..body.layout.items.len())
                    .rev()
                    .find(|&index| under(index))
            });

        // A branch that lost its name to a neighbour's still ends in something: a bud in the
        // stroke's own ink at its tip, so no stroke trails off into nothing. Where the style
        // already grows a leaf there, the leaf is the ending.
        let mut path = pixelkit_raster::Path::new();
        let heaviest = paint::max_weight(&body.layout);
        for (index, item) in body.layout.items.iter().enumerate() {
            let layouts::Shape::Stroke { points, widths } = &item.shape else {
                continue;
            };
            let leaf = body
                .layout
                .items
                .get(index + 1)
                .is_none_or(|next| next.depth <= item.depth);
            let named = labels.iter().any(|(labelled, _)| *labelled == index);
            let grown = style.leaves && !item.is_dir;
            let Some(&(x, y)) = points.last() else {
                continue;
            };
            if !leaf || named || grown {
                continue;
            }
            let tip = widths.last().copied().unwrap_or(1.0);
            path.clear();
            path.circle(
                rect.x as f32 + x * scale,
                rect.y as f32 + y * scale,
                (tip * 0.5 + 1.0).max(2.0) * scale,
            );
            ui.painter.fill_path(
                ui.kernel,
                &mut path,
                paint::bark(&palette, style, item, heaviest),
                255,
            );
        }

        // State marks over the material: what arrived, and what is under the pointer. On a
        // filled shape an arrival is an accent rim and hover a faint wash — a shape can be most
        // of the body, and a body washed in the accent is loud; a stroke is thin enough to take
        // the accent nearly whole.
        path.clear();
        let origin = (rect.x as f32, rect.y as f32);
        for (index, item) in body.layout.items.iter().enumerate() {
            let thin = matches!(item.shape, layouts::Shape::Stroke { .. });
            let (hover, bloom) = (Some(index) == hovered, is_bloomed(index));
            if !hover && !bloom {
                continue;
            }
            path.clear();
            paint::shape_path(&mut path, &item.shape, scale, origin, style.gap);
            if thin {
                let alpha = if bloom { 210 } else { 130 };
                ui.painter
                    .fill_path(ui.kernel, &mut path, design::accent(), alpha);
                continue;
            }
            if hover {
                ui.painter
                    .fill_path(ui.kernel, &mut path, design::accent(), 30);
            }
            if bloom {
                paint::shape_path(&mut path, &item.shape, scale, origin, style.gap + 3.0);
                ui.painter.fill_path_rule(
                    ui.kernel,
                    &mut path,
                    pixelkit_raster::FillRule::EvenOdd,
                    design::accent(),
                    220,
                );
            }
        }

        let surface = design::folder_shell();
        for (index, label) in &labels {
            let item = &body.layout.items[*index];
            // A name beside a stroke is set on a knockout of the tile's own surface, as a map
            // sets a name over a road: a branch that passes behind it never cuts a word.
            if matches!(item.shape, layouts::Shape::Stroke { .. }) {
                let knockout = Rect::new(
                    label.x - ui.px(2),
                    label.y + ui.px(1),
                    label.w + ui.px(3),
                    label.h - ui.px(2),
                );
                ui.painter
                    .fill_rounded_rect_aa(ui.kernel, knockout, ui.px(3) as f32, surface, 225);
            }
            let colour = if is_bloomed(*index) || Some(*index) == hovered {
                design::accent()
            } else if item.is_dir && item.depth == 0 {
                design::text()
            } else {
                design::text_dim()
            };
            ui.label_styled(*label, &body.names[*index], style_text, colour, Align::Left);
        }

        if let Some(index) = hovered
            && !body.paths[index].is_empty()
            && ui.input.take_click(rect)
        {
            let path = root.join(&body.paths[index]);
            actions.push(if body.layout.items[index].is_dir {
                FieldAction::OpenInFiles(path)
            } else {
                FieldAction::OpenFile(path)
            });
        }
        true
    }
}

/// Whether two label boxes collide, allowing their edges to touch.
fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, is_dir: bool, score: f32, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            name: name.to_owned(),
            is_dir,
            score,
            children,
        }
    }

    fn count(nodes: &[WeightedNode]) -> usize {
        nodes.iter().map(|node| 1 + count(&node.children)).sum()
    }

    fn depth(nodes: &[WeightedNode]) -> u8 {
        nodes
            .iter()
            .map(|node| 1 + depth(&node.children))
            .max()
            .unwrap_or(0)
    }

    /// A wide project tree: 4 top-level directories, each with 6 children with 3 files each.
    fn project() -> Vec<TreeNode> {
        (0..4)
            .map(|top| {
                node(
                    &format!("dir{top}"),
                    true,
                    10.0 + top as f32,
                    (0..6)
                        .map(|mid| {
                            node(
                                &format!("sub{mid}"),
                                true,
                                2.0 + mid as f32,
                                (0..3)
                                    .map(|leaf| {
                                        node(
                                            &format!("f{leaf}.rs"),
                                            false,
                                            0.5 + leaf as f32,
                                            vec![],
                                        )
                                    })
                                    .collect(),
                            )
                        })
                        .collect(),
                )
            })
            .collect()
    }

    /// A bigger body is budgeted more of the tree, and whatever the size the cut stays inside
    /// the budget: no more nodes than it allows, no deeper than it allows.
    #[test]
    fn a_body_shows_no_more_of_the_tree_than_its_area_affords() {
        let tree = project();
        let wide = Budget::for_area(350.0, 200.0);
        let dominant = Budget::for_area(530.0, 300.0);
        assert!(dominant.max_nodes > wide.max_nodes);
        assert!(dominant.max_depth >= wide.max_depth);
        assert!((2..=5).contains(&wide.max_depth) && (2..=5).contains(&dominant.max_depth));
        for budget in [
            Budget::for_area(120.0, 60.0),
            wide,
            dominant,
            Budget::for_area(2000.0, 2000.0),
        ] {
            let roots = weighted_tree(&tree, budget);
            assert!(count(&roots) <= budget.max_nodes, "{budget:?}");
            assert!(depth(&roots) <= budget.max_depth, "{budget:?}");
        }
        assert_eq!(
            count(&weighted_tree(&tree, Budget::for_area(2000.0, 2000.0))),
            4 + 24 + 72
        );
    }

    /// What a tight budget drops is the weakest leaves — a whole weak subtree goes before a
    /// strong one loses anything — and the id of what survives is its path, not its position.
    #[test]
    fn the_cut_drops_the_weakest_leaves_first() {
        let tree = vec![
            node(
                "strong",
                true,
                9.0,
                vec![
                    node("a.rs", false, 5.0, vec![]),
                    node("b.rs", false, 4.0, vec![]),
                ],
            ),
            node("weak", true, 1.0, vec![node("c.rs", false, 0.2, vec![])]),
            node("empty", true, 0.0, vec![]),
        ];
        let budget = Budget {
            max_nodes: 3,
            max_depth: 3,
        };
        let roots = weighted_tree(&tree, budget);
        let names: Vec<&str> = roots.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(
            names,
            ["strong"],
            "c.rs, then the leafless weak, went first"
        );
        assert_eq!(roots[0].children.len(), 2);
        assert_eq!(roots[0].children[0].id, layouts::node_id("strong/a.rs"));
    }
}
