//! Homegarden Place layouts: the *shape* of a Place's used-structure tree drawn as one of several
//! themeable structures — strip and squarified (cushion) treemaps, weighted Voronoi treemaps,
//! circle packing, sunburst and icicle partitions, space-colonization branches, a weighted
//! Pythagoras/L-system fractal and a folio of stacked sheets.
//!
//! The grammar the shell follows is *size = relevance, position = memory*: every layout here
//! orders siblings by their stable [`node_id`] (never by weight), so a sibling keeps its place
//! while relevance weights drift; the seeded kinds draw their jitter from `seed ^ node.id` and
//! the Voronoi and branch kinds re-seed from a `previous` layout when one is given.
//!
//! Every layout is pure arithmetic: the same [`LayoutRequest`] always yields the same
//! [`GardenLayout`], with every coordinate inside the request's [`Bounds`].

mod branches;
mod circles;
mod folio;
mod fractal;
mod geometry;
mod radial;
mod rects;
mod voronoi;

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// One node of a Place's used-structure tree, weighted by relevance.
#[derive(Clone, Debug, PartialEq)]
pub struct WeightedNode {
    /// Stable identity: [`node_id`] of the node's path relative to the Place root.
    pub id: u64,
    pub name: String,
    /// Relevance weight; nodes with a weight ≤ 0 (or non-finite) are skipped with their subtree.
    pub weight: f32,
    pub is_dir: bool,
    pub children: Vec<WeightedNode>,
}

/// Stable node identity: 64-bit FNV-1a of the node's path relative to the Place root.
pub fn node_id(relative: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    relative.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// Which structure a Place is drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LayoutKind {
    /// Ordered strip treemap (Bederson, Shneiderman & Wattenberg 2002), flat rectangles.
    Strip,
    /// Squarified treemap (Bruls, Huizing & van Wijk 2000) in stable order, with cushions.
    Squarified,
    /// Weighted Voronoi treemap (power diagram, Nocaj & Brandes 2012), convex polygons.
    Voronoi,
    /// Hierarchical circle packing (front-chain, Wang et al. 2006).
    CirclePack,
    /// Radial partition around the Place's centre.
    Sunburst,
    /// Top-down rectangular partition.
    Icicle,
    /// Space-colonization branches (Runions, Lane & Prusinkiewicz 2007) with pipe-model widths.
    Branches,
    /// Weighted Pythagoras-tree / binary L-system strokes.
    Fractal,
    /// Stacked, slightly rotated sheets of paper.
    Folio,
}

/// The rectangle a layout must stay within.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Bounds {
    fn is_usable(self) -> bool {
        [self.x, self.y, self.w, self.h]
            .iter()
            .all(|v| v.is_finite())
            && self.w > 0.0
            && self.h > 0.0
    }

    fn right(self) -> f32 {
        self.x + self.w
    }

    fn bottom(self) -> f32 {
        self.y + self.h
    }
}

/// Everything one layout depends on.
#[derive(Clone, Copy, Debug)]
pub struct LayoutRequest<'a> {
    pub kind: LayoutKind,
    /// The Place's top-level entries.
    pub roots: &'a [WeightedNode],
    pub bounds: Bounds,
    /// The Place's material seed; per-node randomness is drawn from `seed ^ node.id`.
    pub seed: u64,
    /// Deepest level laid out; roots are depth 0, so `0` lays out the roots only.
    pub max_depth: u8,
    /// Item budget; the weakest leaves are dropped first until the tree fits.
    pub max_nodes: usize,
    /// The previous layout of the same Place, to keep positions (Voronoi sites, branch tips).
    pub previous: Option<&'a GardenLayout>,
}

/// The geometry of one item. Coordinates are absolute, in the same space as [`Bounds`].
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// `surface` holds van Wijk cushion coefficients `[sx1, sx2, sy1, sy2]` (zero when flat).
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        surface: [f32; 4],
    },
    /// Convex or simple polygon, clockwise on screen (y down).
    Polygon {
        points: Vec<(f32, f32)>,
    },
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
    },
    /// Annular sector; angles in radians on screen (y down, so increasing angles turn
    /// clockwise), `a0 < a1`.
    Sector {
        cx: f32,
        cy: f32,
        r0: f32,
        r1: f32,
        a0: f32,
        a1: f32,
    },
    /// Polyline with a per-vertex width (tapering); `widths.len() == points.len()`.
    Stroke {
        points: Vec<(f32, f32)>,
        widths: Vec<f32>,
    },
}

/// One laid-out node.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub node: u64,
    pub depth: u8,
    pub weight: f32,
    pub is_dir: bool,
    pub shape: Shape,
    /// Label room: `(left x, vertical centre y, max width)`, sized for one ~14 px text line;
    /// `None` when there is no room.
    pub label: Option<(f32, f32, f32)>,
}

/// A laid-out Place. `items` are in paint order.
#[derive(Clone, Debug, PartialEq)]
pub struct GardenLayout {
    pub kind: LayoutKind,
    pub bounds: Bounds,
    pub items: Vec<Item>,
}

/// Lay out a Place's tree as the requested structure.
pub fn layout(request: &LayoutRequest<'_>) -> GardenLayout {
    let mut out = GardenLayout {
        kind: request.kind,
        bounds: request.bounds,
        items: Vec::new(),
    };
    if !request.bounds.is_usable() {
        return out;
    }
    let forest = Forest::prepare(request.roots, request.max_depth, request.max_nodes);
    if forest.roots.is_empty() {
        return out;
    }
    let previous = request
        .previous
        .filter(|previous| previous.kind == request.kind);
    let ctx = Ctx {
        forest: &forest,
        bounds: request.bounds,
        seed: request.seed,
        previous,
    };
    out.items = match request.kind {
        LayoutKind::Strip => rects::strip(&ctx),
        LayoutKind::Squarified => rects::squarified(&ctx),
        LayoutKind::Icicle => rects::icicle(&ctx),
        LayoutKind::Voronoi => voronoi::layout(&ctx),
        LayoutKind::CirclePack => circles::layout(&ctx),
        LayoutKind::Sunburst => radial::sunburst(&ctx),
        LayoutKind::Branches => branches::layout(&ctx),
        LayoutKind::Fractal => fractal::layout(&ctx),
        LayoutKind::Folio => folio::layout(&ctx),
    };
    for item in &mut out.items {
        item.label = item
            .label
            .and_then(|label| clamp_label(label, request.bounds));
    }
    out
}

/// Shared inputs of one layout pass.
struct Ctx<'a> {
    forest: &'a Forest,
    bounds: Bounds,
    seed: u64,
    previous: Option<&'a GardenLayout>,
}

impl Ctx<'_> {
    fn rng(&self, id: u64) -> Rng {
        Rng::new(self.seed ^ id)
    }

    fn item(&self, index: usize, shape: Shape, label: Option<(f32, f32, f32)>) -> Item {
        let node = &self.forest.nodes[index];
        Item {
            node: node.id,
            depth: node.depth,
            weight: node.weight,
            is_dir: node.is_dir,
            shape,
            label,
        }
    }

    fn previous_shape(&self, id: u64) -> Option<&Shape> {
        self.previous?
            .items
            .iter()
            .find(|item| item.node == id)
            .map(|item| &item.shape)
    }
}

/// The request's tree after filtering, depth trimming, budget pruning and id ordering.
#[derive(Debug)]
struct Forest {
    /// Preorder; children of every node sorted by `(id, name)`.
    nodes: Vec<FNode>,
    roots: Vec<usize>,
}

#[derive(Debug)]
struct FNode {
    id: u64,
    weight: f32,
    is_dir: bool,
    depth: u8,
    children: Vec<usize>,
    /// Length of the name in characters, to estimate label boxes.
    name_chars: usize,
}

/// Arena entry used while pruning.
struct Draft<'a> {
    source: &'a WeightedNode,
    depth: u8,
    parent: Option<usize>,
    children: Vec<usize>,
    live_children: usize,
    alive: bool,
}

/// A prunable leaf in the budget heap: the *weakest* (lowest weight, then highest id) pops first.
struct Weakest {
    weight: f32,
    id: u64,
    index: usize,
}

impl PartialEq for Weakest {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Weakest {}

impl PartialOrd for Weakest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Weakest {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: "greater" means "weaker".
        other
            .weight
            .total_cmp(&self.weight)
            .then(self.id.cmp(&other.id))
            .then(self.index.cmp(&other.index))
    }
}

impl Forest {
    fn prepare(roots: &[WeightedNode], max_depth: u8, max_nodes: usize) -> Self {
        let mut drafts: Vec<Draft<'_>> = Vec::new();
        let mut top = Vec::new();
        for root in sorted(roots) {
            if let Some(index) = Self::draft(root, 0, None, max_depth, &mut drafts) {
                top.push(index);
            }
        }

        let mut live = drafts.len();
        if live > max_nodes {
            let mut heap: BinaryHeap<Weakest> = drafts
                .iter()
                .enumerate()
                .filter(|(_, d)| d.children.is_empty())
                .map(|(index, d)| Weakest {
                    weight: d.source.weight,
                    id: d.source.id,
                    index,
                })
                .collect();
            while live > max_nodes {
                let Some(weakest) = heap.pop() else { break };
                drafts[weakest.index].alive = false;
                live -= 1;
                if let Some(parent) = drafts[weakest.index].parent {
                    drafts[parent].live_children -= 1;
                    if drafts[parent].live_children == 0 {
                        heap.push(Weakest {
                            weight: drafts[parent].source.weight,
                            id: drafts[parent].source.id,
                            index: parent,
                        });
                    }
                }
            }
        }

        let mut forest = Forest {
            nodes: Vec::with_capacity(live),
            roots: Vec::new(),
        };
        for index in top {
            if let Some(kept) = forest.compact(&drafts, index) {
                forest.roots.push(kept);
            }
        }
        forest
    }

    fn draft<'a>(
        node: &'a WeightedNode,
        depth: u8,
        parent: Option<usize>,
        max_depth: u8,
        drafts: &mut Vec<Draft<'a>>,
    ) -> Option<usize> {
        if !(node.weight.is_finite() && node.weight > 0.0) || depth > max_depth {
            return None;
        }
        let index = drafts.len();
        drafts.push(Draft {
            source: node,
            depth,
            parent,
            children: Vec::new(),
            live_children: 0,
            alive: true,
        });
        if depth < max_depth {
            for child in sorted(&node.children) {
                if let Some(child_index) =
                    Self::draft(child, depth + 1, Some(index), max_depth, drafts)
                {
                    drafts[index].children.push(child_index);
                }
            }
        }
        drafts[index].live_children = drafts[index].children.len();
        Some(index)
    }

    fn compact(&mut self, drafts: &[Draft<'_>], index: usize) -> Option<usize> {
        let draft = &drafts[index];
        if !draft.alive {
            return None;
        }
        let kept = self.nodes.len();
        self.nodes.push(FNode {
            id: draft.source.id,
            weight: draft.source.weight,
            is_dir: draft.source.is_dir,
            depth: draft.depth,
            children: Vec::new(),
            name_chars: draft.source.name.chars().count(),
        });
        let mut children = Vec::with_capacity(draft.live_children);
        for &child in &draft.children {
            if let Some(kept_child) = self.compact(drafts, child) {
                children.push(kept_child);
            }
        }
        self.nodes[kept].children = children;
        Some(kept)
    }

    fn weight(&self, index: usize) -> f64 {
        f64::from(self.nodes[index].weight)
    }

    /// Deepest depth present plus one.
    fn levels(&self) -> usize {
        self.nodes
            .iter()
            .map(|n| usize::from(n.depth) + 1)
            .max()
            .unwrap_or(0)
    }
}

/// Siblings in stable order: by id, then name (ids are path hashes, so ties are collisions).
fn sorted(nodes: &[WeightedNode]) -> Vec<&WeightedNode> {
    let mut out: Vec<&WeightedNode> = nodes.iter().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.name.cmp(&b.name)));
    out
}

/// SplitMix64: tiny, seedable, platform-independent.
#[derive(Clone, Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }

    /// Uniform in `[lo, hi)`.
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

/// Height of one label line (11 px text on a ~14 px line).
const LABEL_LINE: f32 = 14.0;
/// Narrowest label worth drawing.
const LABEL_MIN_W: f32 = 28.0;
/// Horizontal breathing room inside a shape.
const LABEL_PAD: f32 = 4.0;

/// Label room centred horizontally on `cx` at vertical centre `cy`, `width` wide.
fn centred_label(cx: f32, cy: f32, width: f32) -> Option<(f32, f32, f32)> {
    let width = width - 2.0 * LABEL_PAD;
    (width >= LABEL_MIN_W).then_some((cx - width / 2.0, cy, width))
}

/// Label room for a rectangle: its first line for a directory with children, else its centre.
fn rect_label(x: f32, y: f32, w: f32, h: f32, has_children: bool) -> Option<(f32, f32, f32)> {
    if h < LABEL_LINE {
        return None;
    }
    let cy = if has_children {
        y + LABEL_LINE / 2.0
    } else {
        y + h / 2.0
    };
    centred_label(x + w / 2.0, cy, w)
}

/// Widest label at a branch tip.
const TIP_LABEL_MAX: f32 = 96.0;

/// Estimated advance of one character of 11 px label text, generous so estimates rarely fall
/// short of the drawn name.
const LABEL_CHAR_W: f32 = 6.5;

/// Label rooms at the tips of the stroke `items` (their forest `nodes` alongside), for text drawn
/// left-aligned from the room's left edge. Each tip tries the side its branch ends heading
/// towards first, then the others (right of, above, below or ending at the tip), and takes the
/// first whose estimated name box stays in the bounds and clear of every name already placed
/// (in paint order, as the shell places them); when none is clear it keeps the first that fits.
/// So a name is dropped for crowding only when all four sides of its tip are taken.
fn place_tip_labels(ctx: &Ctx<'_>, items: &mut [Item], nodes: &[usize]) {
    let b = ctx.bounds;
    let half = LABEL_LINE / 2.0;
    let mut placed: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(items.len());
    for (item, &index) in items.iter_mut().zip(nodes) {
        let Shape::Stroke { points, .. } = &item.shape else {
            continue;
        };
        let (x, y) = points[points.len() - 1];
        let (dx, dy) = heading(points);
        let chars = ctx.forest.nodes[index].name_chars as f32;
        let estimate = (chars * LABEL_CHAR_W + 2.0).clamp(LABEL_MIN_W, TIP_LABEL_MAX.min(b.w));
        let above = y - LABEL_LINE * 0.7;
        let below = y + LABEL_LINE * 0.7;
        // Over the tip, slid left only as far as the right edge demands.
        let beside = (x - LABEL_PAD).min(b.right() - estimate).max(b.x);
        // (left edge, centre y, room width, estimated box width)
        let right = (x + LABEL_PAD, y, TIP_LABEL_MAX, estimate);
        let left = (x - LABEL_PAD - estimate, y, estimate, estimate);
        let up = (beside, above, TIP_LABEL_MAX, estimate);
        let down = (beside, below, TIP_LABEL_MAX, estimate);
        let order = if dx > 0.5 {
            [right, up, down, left]
        } else if dx < -0.5 {
            [left, up, down, right]
        } else if dy <= 0.0 {
            [up, right, left, down]
        } else {
            [down, right, left, up]
        };
        let fits = |&(lx, cy, _, w): &(f32, f32, f32, f32)| {
            lx >= b.x && lx + w <= b.right() && cy - half >= b.y && cy + half <= b.bottom()
        };
        let clear = |&(lx, cy, _, w): &(f32, f32, f32, f32)| {
            placed.iter().all(|&(px, py, pw, ph)| {
                lx + w <= px || px + pw <= lx || cy + half <= py || py + ph <= cy - half
            })
        };
        let chosen = order
            .iter()
            .find(|c| fits(c) && clear(c))
            .or_else(|| order.iter().find(|c| fits(c)))
            .copied();
        item.label = chosen.map(|(lx, cy, room, w)| {
            placed.push((lx, cy - half, w, LABEL_LINE));
            (lx, cy, room)
        });
    }
}

/// The unit direction of a polyline's last segment (straight up if it has none).
fn heading(points: &[(f32, f32)]) -> (f32, f32) {
    points
        .windows(2)
        .rev()
        .find_map(|w| {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            let len = dx.hypot(dy);
            (len > 1e-6).then(|| (dx / len, dy / len))
        })
        .unwrap_or((0.0, -1.0))
}

/// Keep label room inside the bounds: a full line high, at least [`LABEL_MIN_W`] wide.
fn clamp_label((x, y, w): (f32, f32, f32), b: Bounds) -> Option<(f32, f32, f32)> {
    let half = LABEL_LINE / 2.0;
    if !(x.is_finite() && y.is_finite() && w.is_finite()) || b.h < LABEL_LINE {
        return None;
    }
    let y = y.clamp(b.y + half, b.bottom() - half);
    let left = x.max(b.x);
    let width = (x + w).min(b.right()) - left;
    (width >= LABEL_MIN_W).then_some((left, y, width))
}
