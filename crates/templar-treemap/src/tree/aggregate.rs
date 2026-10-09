//! Bottom-up size and composition totals, and the size-descending sibling order the squarified
//! layout expects. Both rely on the arena invariant that children follow their parents.

use super::arena::{FileTree, NodeId};
use super::extensions::CATEGORY_COUNT;

/// Compute aggregated sizes for all directory nodes (bottom-up).
/// After this, each directory's `size` field equals the sum of all descendant file sizes.
pub fn aggregate_sizes(tree: &mut FileTree) {
    // Process nodes in reverse order (children before parents) since
    // children always have higher indices than their parents in our arena.
    // This is guaranteed by the add_child insertion order.
    let len = tree.nodes.len();
    for i in (0..len).rev() {
        let node = &tree.nodes[i];
        if !node.is_dir {
            continue;
        }

        // Sum up all direct children
        let mut total = tree.directory_bytes[i];
        let mut weights = [0_u64; CATEGORY_COUNT];
        weights[super::extensions::category_index(super::extensions::FileCategory::Misc)] = total;
        let mut child = node.first_child;
        while let Some(child_id) = child {
            total += tree.nodes[child_id.index()].size;
            let child_weights = tree.category_weights[child_id.index()];
            for idx in 0..CATEGORY_COUNT {
                weights[idx] += child_weights[idx];
            }
            child = tree.nodes[child_id.index()].next_sibling;
        }
        tree.nodes[i].size = total;
        tree.category_weights[i] = weights;
    }
}

/// Sort children of each directory by size (descending).
/// The squarified layout algorithm expects children sorted by size.
/// This re-links the sibling list without moving nodes in the arena.
pub fn sort_children_by_size(tree: &mut FileTree) {
    let len = tree.nodes.len();
    for i in 0..len {
        if !tree.nodes[i].is_dir || tree.nodes[i].first_child.is_none() {
            continue;
        }

        // Collect children into a vec
        let mut children: Vec<NodeId> = Vec::new();
        let mut child = tree.nodes[i].first_child;
        while let Some(child_id) = child {
            children.push(child_id);
            child = tree.nodes[child_id.index()].next_sibling;
        }

        // Sort by size descending
        children.sort_by(|a, b| tree.nodes[b.index()].size.cmp(&tree.nodes[a.index()].size));

        // Re-link the sibling list
        if children.is_empty() {
            continue;
        }
        tree.nodes[i].first_child = Some(children[0]);
        for w in children.windows(2) {
            tree.nodes[w[0].index()].next_sibling = Some(w[1]);
        }
        tree.nodes[children.last().unwrap().index()].next_sibling = None;
    }
}
