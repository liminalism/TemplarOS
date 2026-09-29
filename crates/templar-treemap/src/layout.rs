//! Byte-proportional squarified layout (Bruls, Huizing & van Wijk 2000) with the cushion ridge
//! accumulation of van Wijk & van de Wetering 1999, level-of-detail culling, coverage
//! redistribution and single-directory chain collapse. Area is bytes and only bytes; descendant
//! file count orders equal-sized siblings and nothing else.

use crate::tree::arena::{FileTree, NodeId};
use std::collections::HashMap;

/// A positioned rectangle in the treemap layout.
#[derive(Debug, Clone, Copy)]
pub struct LayoutRect {
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub depth: u16,
    /// Accumulated cushion surface coefficients [sx1, sx2, sy1, sy2]
    /// (linear_x, quad_x, linear_y, quad_y) from van Wijk & van de Wetering 1999.
    pub surface: [f32; 4],
}

/// The full layout result (rects + fast lookup).
#[derive(Debug, Default)]
pub struct Layout {
    /// All visible rectangles (files + directories for interaction)
    pub rects: Vec<LayoutRect>,
    /// node → index into `rects` (O(1) hover, tooltip, highlighting)
    pub node_to_rect: HashMap<NodeId, usize>,
}

/// Configuration for treemap layout.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutConfig {
    /// Minimum screen area (px²) to render a node (LOD culling)
    pub min_area: f32,
    /// Minimum side length (px) to render a node.
    pub min_side: f32,
    /// Minimum side length (px) required before recursing into a directory.
    pub recurse_min_side: f32,
    /// Base padding between siblings (px)
    pub padding: f32,
    /// How much padding shrinks per nesting level (0.0 = constant, 0.7 = nice taper)
    pub padding_falloff: f32,
    /// Directory frame thickness in px (children are inset inside this frame).
    pub dir_frame_px: f32,
    /// Directory header-band height in px (used for label/navigation affordance).
    pub dir_header_px: f32,
    /// Per-level decay for frame/header dimensions.
    pub dir_frame_falloff: f32,
    /// Maximum recursion depth (safety + performance)
    pub max_depth: u16,
    /// Target fractional area coverage to keep per directory before truncating tiny children.
    /// Remaining tail is redistributed to kept items to avoid interior "empty" regions.
    pub child_coverage_target: f64,
    /// Hard cap on visible children per directory to avoid pathological stripe explosions.
    pub max_children_per_dir: usize,
    /// Target aspect ratio for squarified layout (1.0 = square-ish)
    pub aspect_tolerance: f64,
    /// Initial cushion ridge height (paper default: 0.5)
    pub cushion_height: f32,
    /// Per-level height decay factor (paper default: 0.75)
    pub cushion_falloff: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            min_area: 49.0,         // Avoid tiny visual noise on million-node trees
            min_side: 6.0,          // Suppress thin strips that are not interactable
            recurse_min_side: 28.0, // Recurse only when child rect can show structure
            padding: 0.0,           // Paper-style treemap has no forced gaps
            padding_falloff: 1.0,
            dir_frame_px: 2.0,
            dir_header_px: 16.0,
            dir_frame_falloff: 0.92,
            max_depth: 64,
            child_coverage_target: 0.995, // Keep 99.5% of each directory's area before truncation
            max_children_per_dir: 1200,   // Prevent extreme stripe counts in very wide folders
            aspect_tolerance: 1.0,
            cushion_height: 0.8, // Increased from 0.5 for more visible cushion effect
            cushion_falloff: 0.75,
        }
    }
}

/// Add a ridge to the cushion surface coefficients along one axis.
/// Matches the CTM procedure from van Wijk & van de Wetering 1999.
pub(crate) fn add_ridge(x1: f32, x2: f32, h: f32, s1: &mut f32, s2: &mut f32) {
    let denom = x2 - x1;
    if denom.abs() < 1e-6 {
        return;
    }
    *s1 += 4.0 * h * (x2 + x1) / denom;
    *s2 -= 4.0 * h / denom;
}

/// Compute layout for any subtree inside an explicit viewport rectangle.
pub fn compute_layout_in_rect(
    tree: &FileTree,
    root: NodeId,
    viewport_x: f32,
    viewport_y: f32,
    viewport_w: f32,
    viewport_h: f32,
    config: &LayoutConfig,
) -> Layout {
    let mut rects = Vec::with_capacity(tree.len() / 4); // rough estimate
    let mut node_to_rect = HashMap::with_capacity(rects.capacity());
    // File counts never affect rectangle area. They are precomputed once and
    // used only as a secondary ordering key when sibling byte sizes are equal.
    let subtree_file_counts = compute_subtree_file_counts(tree);

    let root_rect = LayoutRect {
        node: root,
        x: viewport_x,
        y: viewport_y,
        w: viewport_w,
        h: viewport_h,
        depth: 0,
        surface: [0.0; 4],
    };

    rects.push(root_rect);
    node_to_rect.insert(root, 0);

    if tree.get(root).is_dir {
        layout_children(
            tree,
            root,
            viewport_x,
            viewport_y,
            viewport_w,
            viewport_h,
            0,
            [0.0; 4],
            config.cushion_height,
            config,
            &mut rects,
            &mut node_to_rect,
            &subtree_file_counts,
        );
    }

    Layout {
        rects,
        node_to_rect,
    }
}

/// Recursively layout children (now with sorting + better culling).
fn layout_children(
    tree: &FileTree,
    parent: NodeId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    depth: u16,
    parent_surface: [f32; 4],
    cushion_h: f32,
    config: &LayoutConfig,
    rects: &mut Vec<LayoutRect>,
    node_to_rect: &mut HashMap<NodeId, usize>,
    subtree_file_counts: &[u64],
) {
    if depth >= config.max_depth {
        return;
    }

    let level_scale = if depth == 0 {
        1.0
    } else {
        config.dir_frame_falloff.powi((depth - 1) as i32)
    };

    // Dynamic padding that tapers with depth
    let pad = if depth == 0 {
        0.0
    } else {
        config.padding * config.padding_falloff.powi(depth as i32)
    };
    // Reserve a visible directory frame + top header band so parent/child nesting is obvious.
    let frame = if depth == 0 {
        0.0
    } else {
        (config.dir_frame_px * level_scale).max(1.0)
    };
    let header = if depth == 0 {
        0.0
    } else {
        (config.dir_header_px * level_scale).min((h * 0.22).max(0.0))
    };

    let inset_x = pad + frame;
    let inset_y = pad + frame;
    let inner_x = x + inset_x;
    let inner_y = y + inset_y + header;
    let inner_w = (w - 2.0 * inset_x).max(0.0);
    let inner_h = (h - 2.0 * inset_y - header).max(0.0);

    if inner_w * inner_h < config.min_area {
        return;
    }

    let parent_size = tree.get(parent).size as f64;
    if parent_size <= 0.0 {
        return;
    }

    // Chain-compression: if one directory dominates almost all bytes of this parent,
    // recurse directly into it using the full parent rectangle to avoid barcode-like strips.
    if let Some((dom_child, dom_ratio, sibling_ratio)) =
        dominant_dir_child(tree, parent, parent_size)
        && dom_ratio >= 0.98
        && sibling_ratio <= 0.02
    {
        {
            let (dom_child, collapsed_levels) = collapse_single_dir_chain(tree, dom_child);
            let child_depth = depth
                .saturating_add(1)
                .saturating_add(collapsed_levels as u16);
            let cx = inner_x;
            let cy = inner_y;
            let cw = inner_w;
            let ch = inner_h;

            let [mut sx1, mut sx2, mut sy1, mut sy2] = parent_surface;
            add_ridge(cx, cx + cw, cushion_h, &mut sx1, &mut sx2);
            add_ridge(cy, cy + ch, cushion_h, &mut sy1, &mut sy2);
            let surface = [sx1, sx2, sy1, sy2];

            let rect = LayoutRect {
                node: dom_child,
                x: cx,
                y: cy,
                w: cw,
                h: ch,
                depth: child_depth,
                surface,
            };
            let idx = rects.len();
            rects.push(rect);
            node_to_rect.insert(dom_child, idx);

            if cw >= config.recurse_min_side && ch >= config.recurse_min_side {
                layout_children(
                    tree,
                    dom_child,
                    cx,
                    cy,
                    cw,
                    ch,
                    child_depth,
                    surface,
                    cushion_h * config.cushion_falloff,
                    config,
                    rects,
                    node_to_rect,
                    subtree_file_counts,
                );
            }
            return;
        }
    }

    // A disk treemap's visual area must be driven by bytes. File count is only
    // a secondary ordering key for equal-sized siblings; it must never make a
    // smaller directory look larger than a bigger one.
    let total_area = (inner_w as f64) * (inner_h as f64);
    let mut items: Vec<LayoutItem> = tree
        .children(parent)
        .filter_map(|id| {
            let bytes = tree.get(id).size;
            let area = (bytes as f64 / parent_size) * total_area;
            (area.is_finite() && area > 0.0).then_some(LayoutItem {
                node: id,
                bytes,
                file_count: subtree_file_counts[id.index()],
                area,
            })
        })
        .collect();
    items.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| b.file_count.cmp(&a.file_count))
            .then_with(|| a.node.0.cmp(&b.node.0))
    });

    if items.is_empty() {
        return;
    }

    // Keep only the most important children for this level.
    // This intentionally trades tiny-detail fidelity for readability and performance,
    // while preserving visual coverage by redistributing the omitted tail.
    let mut visible: Vec<LayoutItem> =
        Vec::with_capacity(items.len().min(config.max_children_per_dir));
    let mut covered_area = 0.0_f64;
    for (idx, item) in items.iter().enumerate() {
        if visible.len() >= config.max_children_per_dir {
            break;
        }
        let keep = idx == 0
            || visible.len() < 8
            || (covered_area / total_area) < config.child_coverage_target;
        if !keep {
            break;
        }
        visible.push(*item);
        covered_area += item.area;
    }
    if visible.is_empty() {
        visible.push(items[0]);
        covered_area = items[0].area;
    }

    if covered_area <= 0.0 {
        return;
    }

    // Stretch kept items to fill the parent area, avoiding dark "empty" interiors
    // caused by culling tiny tails post-layout.
    let scale = total_area / covered_area;
    for item in &mut visible {
        item.area *= scale;
    }

    let areas: Vec<f64> = visible.iter().map(|item| item.area).collect();

    // Squarified layout
    let positioned = squarify(
        &areas,
        inner_x as f64,
        inner_y as f64,
        inner_w as f64,
        inner_h as f64,
    );

    for (i, pos) in positioned.iter().enumerate() {
        let mut child_id = visible[i].node;
        let mut child_depth = depth.saturating_add(1);
        if tree.get(child_id).is_dir {
            let (collapsed, collapsed_levels) = collapse_single_dir_chain(tree, child_id);
            child_id = collapsed;
            child_depth = child_depth.saturating_add(collapsed_levels as u16);
        }

        let area = (pos.w * pos.h) as f32;
        if area < 1.0 {
            continue;
        }

        let cx = pos.x as f32;
        let cy = pos.y as f32;
        let cw = pos.w as f32;
        let ch = pos.h as f32;
        if cw <= 0.5 || ch <= 0.5 {
            continue;
        }

        // Accumulate cushion ridges from parent
        let [mut sx1, mut sx2, mut sy1, mut sy2] = parent_surface;
        add_ridge(cx, cx + cw, cushion_h, &mut sx1, &mut sx2);
        add_ridge(cy, cy + ch, cushion_h, &mut sy1, &mut sy2);
        let surface = [sx1, sx2, sy1, sy2];

        let rect = LayoutRect {
            node: child_id,
            x: cx,
            y: cy,
            w: cw,
            h: ch,
            depth: child_depth,
            surface,
        };

        let idx = rects.len();
        rects.push(rect);
        node_to_rect.insert(child_id, idx);

        // Recurse only into directories
        if tree.get(child_id).is_dir
            && cw >= config.recurse_min_side
            && ch >= config.recurse_min_side
        {
            layout_children(
                tree,
                child_id,
                cx,
                cy,
                cw,
                ch,
                child_depth,
                surface,
                cushion_h * config.cushion_falloff,
                config,
                rects,
                node_to_rect,
                subtree_file_counts,
            );
        }
    }
}

#[derive(Clone, Copy)]
struct LayoutItem {
    node: NodeId,
    bytes: u64,
    file_count: u64,
    area: f64,
}

/// Count descendant files for every arena node in one bottom-up pass.
///
/// Nodes are appended after their parents, so reverse arena order guarantees
/// that each child's count is available before its directory is processed.
fn compute_subtree_file_counts(tree: &FileTree) -> Vec<u64> {
    let mut counts = vec![0_u64; tree.len()];
    for index in (0..tree.len()).rev() {
        let node = &tree.nodes[index];
        counts[index] = if node.is_dir {
            let mut total = 0_u64;
            let mut child = node.first_child;
            while let Some(child_id) = child {
                total = total.saturating_add(counts[child_id.index()]);
                child = tree.nodes[child_id.index()].next_sibling;
            }
            total
        } else {
            1
        };
    }
    counts
}

fn dominant_dir_child(
    tree: &FileTree,
    parent: NodeId,
    parent_size: f64,
) -> Option<(NodeId, f64, f64)> {
    if parent_size <= 0.0 {
        return None;
    }
    let mut best: Option<(NodeId, u64)> = None;
    let mut total_children = 0u64;
    for child in tree.children(parent) {
        let node = tree.get(child);
        let size = node.size;
        total_children = total_children.saturating_add(size);
        if !node.is_dir {
            continue;
        }
        match best {
            None => best = Some((child, size)),
            Some((_, best_size)) if size > best_size => best = Some((child, size)),
            _ => {}
        }
    }
    let (child_id, child_size) = best?;
    let dom_ratio = child_size as f64 / parent_size;
    let sibling_size = total_children.saturating_sub(child_size);
    let sibling_ratio = sibling_size as f64 / parent_size;
    Some((child_id, dom_ratio, sibling_ratio))
}

/// Collapse a pure single-directory chain (A -> B -> C ...) into its terminal directory.
/// This removes repeated full-rect nesting that otherwise creates stripe-heavy visuals.
fn collapse_single_dir_chain(tree: &FileTree, start: NodeId) -> (NodeId, usize) {
    let mut node = start;
    let mut collapsed = 0usize;
    loop {
        let mut children = tree.children(node);
        let Some(first) = children.next() else {
            break;
        };
        if children.next().is_some() {
            break;
        }
        if !tree.get(first).is_dir {
            break;
        }
        node = first;
        collapsed += 1;
    }
    (node, collapsed)
}

/// Squarified layout following Bruls et al.:
/// keep adding items to the current row while worst-aspect improves.
pub(crate) fn squarify(
    areas: &[f64],
    mut x: f64,
    mut y: f64,
    mut w: f64,
    mut h: f64,
) -> Vec<Positioned> {
    let mut result = Vec::with_capacity(areas.len());
    let sorted = areas;

    let mut idx = 0usize;
    let mut row_start = 0usize;
    let mut row_sum = 0.0;
    let mut row_min = f64::INFINITY;
    let mut row_max = 0.0;

    while idx < sorted.len() {
        if w <= 1e-6 || h <= 1e-6 {
            break;
        }

        let c = sorted[idx];
        let side = w.min(h);
        let current = if row_sum > 0.0 {
            worst_aspect_ratio_stats(row_min, row_max, row_sum, side)
        } else {
            f64::INFINITY
        };
        let next_sum = row_sum + c;
        let next_min = row_min.min(c);
        let next_max = row_max.max(c);
        let next = worst_aspect_ratio_stats(next_min, next_max, next_sum, side);

        // Add to row while aspect ratio improves (or row is empty).
        if row_sum <= 0.0 || next <= current {
            row_sum = next_sum;
            row_min = next_min;
            row_max = next_max;
            idx += 1;
            continue;
        }

        layout_row(
            &sorted[row_start..idx],
            row_sum,
            &mut x,
            &mut y,
            &mut w,
            &mut h,
            &mut result,
        );
        row_start = idx;
        row_sum = 0.0;
        row_min = f64::INFINITY;
        row_max = 0.0;
    }

    if row_sum > 0.0 && row_start < idx {
        layout_row(
            &sorted[row_start..idx],
            row_sum,
            &mut x,
            &mut y,
            &mut w,
            &mut h,
            &mut result,
        );
    }

    result
}

fn layout_row(
    row: &[f64],
    row_sum: f64,
    x: &mut f64,
    y: &mut f64,
    w: &mut f64,
    h: &mut f64,
    out: &mut Vec<Positioned>,
) {
    if row.is_empty() || row_sum <= 0.0 || *w <= 1e-8 || *h <= 1e-8 {
        return;
    }

    // Paper's width(): the shortest side of the remaining rectangle.
    // If width is shortest -> horizontal strip; otherwise vertical strip.
    let horizontal = *w <= *h;
    let short = if horizontal { *w } else { *h };
    let thickness = row_sum / short;
    if !thickness.is_finite() || thickness <= 0.0 {
        return;
    }

    let mut offset = 0.0;
    for (i, &area) in row.iter().enumerate() {
        let mut length = area / thickness;
        if !length.is_finite() || length <= 0.0 {
            continue;
        }
        // Absorb floating point error into final rect in the strip.
        if i == row.len() - 1 {
            let remaining = if horizontal {
                (*w - offset).max(0.0)
            } else {
                (*h - offset).max(0.0)
            };
            if remaining.is_finite() && remaining > 0.0 {
                length = remaining;
            }
        }

        let pos = if horizontal {
            Positioned {
                x: *x + offset,
                y: *y,
                w: length,
                h: thickness,
            }
        } else {
            Positioned {
                x: *x,
                y: *y + offset,
                w: thickness,
                h: length,
            }
        };
        out.push(pos);
        offset += length;
    }

    if horizontal {
        *y += thickness;
        *h = (*h - thickness).max(0.0);
    } else {
        *x += thickness;
        *w = (*w - thickness).max(0.0);
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Positioned {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) w: f64,
    pub(crate) h: f64,
}

fn worst_aspect_ratio_stats(min_r: f64, max_r: f64, sum: f64, side: f64) -> f64 {
    if sum <= 0.0 || side <= 0.0 || min_r <= 0.0 || max_r <= 0.0 {
        return f64::MAX;
    }
    let side_sq = side * side;
    let sum_sq = sum * sum;
    let a = (side_sq * max_r) / sum_sq;
    let b = sum_sq / (side_sq * min_r);
    a.max(b)
}

#[cfg(test)]
mod tests {
    use super::{LayoutConfig, compute_layout_in_rect, squarify};
    use crate::tree::aggregate::aggregate_sizes;
    use crate::tree::arena::{FileNode, FileTree, NodeId};
    use crate::tree::extensions::FileCategory;
    use std::path::PathBuf;

    fn add_dir(tree: &mut FileTree, parent: NodeId, name: &str) -> NodeId {
        tree.add_child(
            parent,
            FileNode {
                name: name.to_owned(),
                size: 0,
                is_dir: true,
                extension_id: 0,
                category: FileCategory::Misc,
                parent: Some(parent),
                first_child: None,
                next_sibling: None,
                depth: 0,
            },
        )
    }

    fn add_file(tree: &mut FileTree, parent: NodeId, name: &str, size: u64) -> NodeId {
        tree.add_child(
            parent,
            FileNode {
                name: name.to_owned(),
                size,
                is_dir: false,
                extension_id: 0,
                category: FileCategory::Misc,
                parent: Some(parent),
                first_child: None,
                next_sibling: None,
                depth: 0,
            },
        )
    }

    fn top_level_test_config() -> LayoutConfig {
        LayoutConfig {
            min_area: 0.0,
            min_side: 0.0,
            recurse_min_side: f32::MAX,
            child_coverage_target: 1.0,
            max_children_per_dir: usize::MAX,
            ..LayoutConfig::default()
        }
    }

    /// Like `top_level_test_config`, but recurses into every directory with no
    /// frame/header/padding insets so nested rectangle areas can be compared
    /// directly against their byte ratios.
    fn nested_test_config() -> LayoutConfig {
        LayoutConfig {
            min_area: 0.0,
            min_side: 0.0,
            recurse_min_side: 0.0,
            padding: 0.0,
            dir_frame_px: 0.0,
            dir_header_px: 0.0,
            child_coverage_target: 1.0,
            max_children_per_dir: usize::MAX,
            ..LayoutConfig::default()
        }
    }

    fn rect_area(layout: &super::Layout, node: NodeId) -> f64 {
        let rect = layout.rects[layout.node_to_rect[&node]];
        f64::from(rect.w) * f64::from(rect.h)
    }

    #[test]
    fn single_item_fills_viewport_without_axis_swap() {
        let rects = squarify(&[1920.0 * 1080.0], 0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(rects.len(), 1);
        let r = rects[0];
        assert!((r.w - 1920.0).abs() < 1e-6);
        assert!((r.h - 1080.0).abs() < 1e-6);
    }

    #[test]
    fn layout_preserves_area_for_simple_case() {
        let areas = [400.0, 300.0, 200.0, 100.0];
        let rects = squarify(&areas, 0.0, 0.0, 50.0, 20.0);
        let total_in: f64 = areas.iter().sum();
        let total_out: f64 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((total_in - total_out).abs() < 1e-6);
    }

    #[test]
    fn top_level_area_is_proportional_to_bytes_not_file_count() {
        let mut tree = FileTree::new("root", PathBuf::from("root"));
        let root = tree.root;
        let large = add_dir(&mut tree, root, "large");
        add_file(&mut tree, large, "large.bin", 300);

        let small_many_files = add_dir(&mut tree, root, "small-many-files");
        for index in 0..20 {
            add_file(
                &mut tree,
                small_many_files,
                &format!("small-{index}.bin"),
                5,
            );
        }
        aggregate_sizes(&mut tree);

        let layout = compute_layout_in_rect(
            &tree,
            root,
            0.0,
            0.0,
            400.0,
            100.0,
            &top_level_test_config(),
        );
        let large_area = rect_area(&layout, large);
        let small_area = rect_area(&layout, small_many_files);

        assert!(large_area > small_area);
        assert!(
            (large_area / small_area - 3.0).abs() < 1e-5,
            "expected 300:100 byte ratio, got areas {large_area}:{small_area}"
        );
    }

    #[test]
    fn equal_byte_sizes_use_descendant_file_count_only_as_order_tiebreaker() {
        let mut tree = FileTree::new("root", PathBuf::from("root"));
        let root = tree.root;

        // Add the many-file directory first. Since add_child prepends, the
        // one-file directory is encountered first and would win a stable
        // size-only sort. The secondary key must deliberately reverse that.
        let many_files = add_dir(&mut tree, root, "many-files");
        for index in 0..10 {
            add_file(&mut tree, many_files, &format!("many-{index}.bin"), 1);
        }
        let one_file = add_dir(&mut tree, root, "one-file");
        add_file(&mut tree, one_file, "one.bin", 10);
        aggregate_sizes(&mut tree);

        let layout = compute_layout_in_rect(
            &tree,
            root,
            0.0,
            0.0,
            200.0,
            100.0,
            &top_level_test_config(),
        );

        assert_eq!(layout.rects[1].node, many_files);
        let many_area = rect_area(&layout, many_files);
        let one_area = rect_area(&layout, one_file);
        assert!(
            (many_area - one_area).abs() < 1e-5,
            "file count must not change equal-byte areas: {many_area} vs {one_area}"
        );
    }

    #[test]
    fn byte_proportionality_holds_recursively_inside_subdirectories() {
        // A big directory (few files) and a small directory (many files), each
        // with children of differing byte sizes. Byte-proportionality must hold
        // both between the top-level dirs and between each dir's own children.
        let mut tree = FileTree::new("root", PathBuf::from("root"));
        let root = tree.root;

        let big = add_dir(&mut tree, root, "big");
        let big_major = add_file(&mut tree, big, "big-major.bin", 600);
        let big_minor = add_file(&mut tree, big, "big-minor.bin", 200); // big = 800

        let small = add_dir(&mut tree, root, "small");
        for index in 0..10 {
            add_file(&mut tree, small, &format!("small-{index}.bin"), 20); // small = 200
        }
        aggregate_sizes(&mut tree);

        let layout =
            compute_layout_in_rect(&tree, root, 0.0, 0.0, 1000.0, 1000.0, &nested_test_config());

        // Top level: 800:200 == 4:1, regardless of the small dir's file count.
        let big_area = rect_area(&layout, big);
        let small_area = rect_area(&layout, small);
        assert!(
            (big_area / small_area - 4.0).abs() < 1e-4,
            "top-level area must track bytes 4:1, got {big_area}:{small_area}"
        );

        // Nested: the big dir's own children must be 600:200 == 3:1, and they
        // must fill their parent's rectangle (no inset in this config).
        let major_area = rect_area(&layout, big_major);
        let minor_area = rect_area(&layout, big_minor);
        assert!(
            (major_area / minor_area - 3.0).abs() < 1e-4,
            "nested children must track bytes 3:1, got {major_area}:{minor_area}"
        );
        // Children cover their parent apart from the mandatory 1px nesting frame
        // (`dir_frame_px.max(1.0)` at depth > 0), so this is a coverage floor
        // rather than an exact-equality check.
        let coverage = (major_area + minor_area) / big_area;
        assert!(
            (0.99..=1.0).contains(&coverage),
            "children must fill (nearly) all of their parent: coverage={coverage}"
        );
    }
}
