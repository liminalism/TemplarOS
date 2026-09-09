//! The forms a Place draws in: shape is the mode of use
//! (`@templaros.decision.a-place-is-a-drawn-structure-not-a-richer-rectangle`).
//!
//! The layout engine keeps allocating rectangles; what is drawn inside one is not a card. A
//! place worked *through* draws a branch — the root as a trunk, the descendants that actually
//! mattered as branches, each taking space in proportion to its use; a place looked *at* draws a
//! shelf; a place that has earned nothing yet draws nothing, and the whitespace is the form.
//!
//! One painter function per form, deliberately: this is the plain, legible drawing — straight
//! strokes, thickness by share, no imagery — and the stylised pass the design conversation wants
//! later replaces a function body here without touching the ledger, the tender or the tile.
//!
//! Which form a place is in is the ledger's answer (`templar-session-core::homegarden::Form`),
//! settled at a tending moment and held for two sessions, so nothing here decides identity from
//! whatever happens to be in the tree this frame.

use std::path::Path;

use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use crate::garden::{GardenPlace, TreeNode};
use crate::surface::design;
use crate::surface::field_screen::FieldAction;

/// The height one named thing needs: below this a band is a stripe with a clipped word in it,
/// which is worse than one fewer branch.
const BAND: i32 = 18;
/// How many branches a place shows at most. The tree is bounded at 32 nodes and sorted by use;
/// past a handful the drawing stops being a glance and becomes a list again.
const BRANCHES: usize = 5;
/// Twigs under one branch, when the band is tall enough to hold them.
const TWIGS: usize = 2;
/// The narrowest cell a shelf item can stand in.
const SHELF_CELL: i32 = 64;

/// Strongest first, then by name so the drawing is stable between frames.
fn ordered(nodes: &[TreeNode]) -> Vec<&TreeNode> {
    let mut ordered: Vec<&TreeNode> = nodes.iter().collect();
    ordered.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.name.cmp(&b.name))
    });
    ordered
}

/// The share each node has of the whole, and the share of the strongest one.
fn shares(nodes: &[&TreeNode]) -> (Vec<f32>, Vec<f32>) {
    let total = nodes.iter().map(|node| node.score.max(0.0)).sum::<f32>();
    let strongest = nodes
        .iter()
        .map(|node| node.score.max(0.0))
        .fold(0.0_f32, f32::max)
        .max(f32::EPSILON);
    let even = 1.0 / nodes.len().max(1) as f32;
    let of_total = nodes
        .iter()
        .map(|node| {
            if total > f32::EPSILON {
                node.score.max(0.0) / total
            } else {
                even
            }
        })
        .collect();
    let of_strongest = nodes
        .iter()
        .map(|node| node.score.max(0.0) / strongest)
        .collect();
    (of_total, of_strongest)
}

fn open(node: &TreeNode, path: &Path) -> FieldAction {
    if node.is_dir {
        FieldAction::OpenInFiles(path.to_path_buf())
    } else {
        FieldAction::OpenFile(path.to_path_buf())
    }
}

/// A branch: the trunk at the left, the used descendants fanning off it, each band as tall as
/// that descendant's share of the use and each stroke as thick, so a place that is a folder of
/// projects reads as its projects and the one that has taken the work is the heavy one.
pub fn branch(
    ui: &mut Ui<'_>,
    rect: Rect,
    root: &Path,
    place: &GardenPlace,
    actions: &mut Vec<FieldAction>,
) {
    let tree = &place.tree;
    let band = ui.px(BAND);
    if rect.h < band || rect.w < ui.px(80) {
        return;
    }
    let nodes = ordered(tree);
    let shown = ((rect.h / band).max(1) as usize).min(BRANCHES);
    let nodes = &nodes[..nodes.len().min(shown)];
    if nodes.is_empty() {
        return;
    }
    let (of_total, of_strongest) = shares(nodes);
    // Proportional bands, floored so the lightest branch still has a name, then scaled back if
    // the floors have overrun the body.
    let mut bands: Vec<i32> = of_total
        .iter()
        .map(|share| band.max((rect.h as f32 * share) as i32))
        .collect();
    let asked: i32 = bands.iter().sum();
    if asked > rect.h {
        for height in &mut bands {
            *height = (*height * rect.h) / asked.max(1);
        }
    }
    let trunk_x = rect.x + ui.px(5);
    let fork_x = rect.x + (rect.w / 4).min(ui.px(52));
    let style = design::state_style(ui);
    let mut y = rect.y;
    let mut trunk: Option<(i32, i32)> = None;
    for ((node, height), thickness) in nodes.iter().zip(&bands).zip(&of_strongest) {
        // The branch sits at the *top* of its band and its twigs hang into the rest of it, so
        // the space a heavy branch is given reads as room around it rather than as a drawing
        // that has slid down the tile.
        let centre = y + band / 2;
        trunk = Some(trunk.map_or((centre, centre), |(top, _)| (top, centre)));
        let width = ui.px(2) as f32 + ui.px(4) as f32 * thickness;
        let path = root.join(&node.name);
        ui.painter.line_aa(
            ui.kernel,
            trunk_x as f32,
            centre as f32,
            fork_x as f32,
            centre as f32,
            width,
            if node.is_dir {
                design::accent()
            } else {
                design::text_faint()
            },
            255,
        );
        let label = Rect::new(
            fork_x + ui.px(8),
            centre - band / 2,
            (rect.right() - fork_x - ui.px(8)).max(0),
            band,
        );
        if ui.input.hovering(label) {
            ui.painter.fill_rect(label, ui.theme.hover);
        }
        ui.label_styled(
            label,
            &node.name,
            style,
            if place.is_bloomed(&path) {
                design::accent()
            } else if node.is_dir {
                design::text()
            } else {
                design::text_dim()
            },
            Align::Left,
        );
        if ui.input.take_click(label) {
            actions.push(open(node, &path));
        }
        twigs(
            ui,
            node,
            &path,
            centre,
            *height,
            (fork_x, rect.right()),
            actions,
        );
        y += height;
    }
    if let Some((top, bottom)) = trunk {
        ui.painter.line_aa(
            ui.kernel,
            trunk_x as f32,
            top as f32,
            trunk_x as f32,
            bottom as f32,
            ui.px(3) as f32,
            design::accent(),
            255,
        );
    }
}

/// What is on one branch: the strongest couple of its children, drawn as stems hanging into the
/// room that branch's own share has given it — so a small place shows its branches and a large
/// one shows what is on them. Nothing is drawn where the band has no room under the name.
fn twigs(
    ui: &mut Ui<'_>,
    node: &TreeNode,
    path: &Path,
    centre: i32,
    height: i32,
    (fork_x, right): (i32, i32),
    actions: &mut Vec<FieldAction>,
) {
    let band = ui.px(BAND);
    let stack = ((height - band) / band).max(0) as usize;
    for (index, twig) in ordered(&node.children)
        .iter()
        .take(stack.min(TWIGS))
        .enumerate()
    {
        let twig_y = centre + band * (index as i32 + 1);
        // The stem starts below its parent's name rather than at the branch, so the drawing
        // never puts a line through a word.
        let stem = (fork_x + ui.px(16)) as f32;
        ui.painter.line_aa(
            ui.kernel,
            stem,
            (centre + band / 2) as f32,
            stem,
            twig_y as f32,
            ui.px(1).max(1) as f32,
            design::text_faint(),
            255,
        );
        ui.painter.line_aa(
            ui.kernel,
            stem,
            twig_y as f32,
            stem + ui.px(10) as f32,
            twig_y as f32,
            ui.px(1).max(1) as f32,
            design::text_faint(),
            255,
        );
        let label = Rect::new(
            fork_x + ui.px(32),
            twig_y - band / 2,
            (right - fork_x - ui.px(32)).max(0),
            band,
        );
        if ui.input.hovering(label) {
            ui.painter.fill_rect(label, ui.theme.hover);
        }
        ui.label_styled(
            label,
            &twig.name,
            design::state_style(ui),
            design::text_dim(),
            Align::Left,
        );
        if ui.input.take_click(label) {
            actions.push(open(twig, &path.join(&twig.name)));
        }
    }
}

/// A shelf: the things the place is looked at for, standing in a row on a rule, each as tall as
/// its share of the looking. Not a grid of thumbnails — no image is read here — but the row and
/// the differing heights are enough to say "this is a place of things", not of structure.
pub fn shelf(
    ui: &mut Ui<'_>,
    rect: Rect,
    root: &Path,
    place: &GardenPlace,
    actions: &mut Vec<FieldAction>,
) {
    let tree = &place.tree;
    let band = ui.px(BAND);
    let cell_floor = ui.px(SHELF_CELL);
    if rect.h < band * 2 || rect.w < cell_floor {
        return;
    }
    let nodes = ordered(tree);
    let shown = ((rect.w / cell_floor).max(1) as usize).min(nodes.len());
    let nodes = &nodes[..shown];
    if nodes.is_empty() {
        return;
    }
    let (_, of_strongest) = shares(nodes);
    let rule = rect.bottom() - band;
    let standing = rule - rect.y;
    let cell_width = rect.w / shown as i32;
    let gap = ui.px(6);
    for (index, (node, share)) in nodes.iter().zip(&of_strongest).enumerate() {
        let height = ((standing as f32) * (0.45 + 0.55 * share)) as i32;
        let cell = Rect::new(
            rect.x + index as i32 * cell_width + gap / 2,
            rule - height,
            (cell_width - gap).max(1),
            height.max(1),
        );
        ui.painter.fill_rounded_rect(
            cell,
            ui.px(design::CHIP_RADIUS),
            if ui.input.hovering(cell) || place.is_bloomed(&root.join(&node.name)) {
                design::accent_soft()
            } else {
                design::well_fill()
            },
        );
        ui.painter
            .stroke_rect(cell, ui.scale.hairline(), design::edge());
        let label = Rect::new(cell.x, rule + ui.px(2), cell.w, band);
        ui.label_styled(
            label,
            &node.name,
            design::state_style(ui),
            design::text_dim(),
            Align::Centre,
        );
        if ui.input.take_click(cell) || ui.input.take_click(label) {
            actions.push(open(node, &root.join(&node.name)));
        }
    }
    ui.painter.fill_rect(
        Rect::new(rect.x, rule, rect.w, ui.scale.hairline().max(1)),
        design::edge_strong(),
    );
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

    #[test]
    fn the_heaviest_branch_is_first_and_ties_are_broken_by_name() {
        let tree = vec![
            node("docs", true, 2.0, Vec::new()),
            node("crates", true, 6.0, Vec::new()),
            node("apps", true, 2.0, Vec::new()),
        ];
        let ordered = ordered(&tree);
        let names: Vec<&str> = ordered.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["crates", "apps", "docs"]);
        let (of_total, of_strongest) = shares(&ordered);
        assert!((of_total.iter().sum::<f32>() - 1.0).abs() < 0.001);
        assert!((of_strongest[0] - 1.0).abs() < f32::EPSILON);
        assert!((of_strongest[1] - 2.0 / 6.0).abs() < 0.001);
    }

    /// A tree with no scores at all — every node evicted down to zero, or a daemon that reports
    /// none — divides the body evenly rather than dividing by zero.
    #[test]
    fn a_tree_with_no_weight_still_shares_the_body_out() {
        let tree = vec![
            node("a", true, 0.0, Vec::new()),
            node("b", false, 0.0, Vec::new()),
        ];
        let (of_total, of_strongest) = shares(&ordered(&tree));
        assert_eq!(of_total, vec![0.5, 0.5]);
        assert!(of_strongest.iter().all(|share| share.is_finite()));
    }
}
