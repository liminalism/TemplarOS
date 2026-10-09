//! The file tree as a flat arena, built from a flat list of scanned entries.

pub mod aggregate;
pub mod arena;
pub mod extensions;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use self::arena::{FileNode, FileTree, NodeId};
use self::extensions::FileCategory;
use crate::scan::RawEntry;

/// The deepest common ancestor of every entry's path (sampled over the first hundred).
fn find_common_root(entries: &[RawEntry]) -> PathBuf {
    let Some(first) = entries.first() else {
        return PathBuf::new();
    };
    let mut root = first.path.clone();
    for entry in entries.iter().skip(1).take(100) {
        while !entry.path.starts_with(&root) {
            match root.parent() {
                Some(parent) => root = parent.to_path_buf(),
                None => return PathBuf::new(),
            }
        }
    }
    root
}

/// Build a [`FileTree`] from scanned entries: directories first, then files under their
/// parents, then sizes aggregated bottom-up and children sorted by size for the layout.
pub fn build_tree(entries: &[RawEntry]) -> FileTree {
    if entries.is_empty() {
        return FileTree::new("(empty)", PathBuf::new());
    }
    let root_path = find_common_root(entries);
    let root_name = root_path.file_name().map_or_else(
        || root_path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut tree = FileTree::new(&root_name, root_path.clone());
    let mut path_map: HashMap<PathBuf, NodeId> = HashMap::new();
    path_map.insert(root_path.clone(), tree.root);

    for entry in entries.iter().filter(|e| e.is_dir) {
        let id = ensure_node(&mut tree, &mut path_map, &entry.path);
        tree.directory_bytes[id.index()] = entry.size;
    }
    for entry in entries.iter().filter(|e| !e.is_dir) {
        let classification = extensions::classify_path(&entry.path);
        let ext_id = classification
            .normalized_extension
            .as_deref()
            .map_or(0, |ext| tree.intern_extension(ext));
        let name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parent_path = entry.path.parent().unwrap_or(Path::new(""));
        let parent_id = match path_map.get(parent_path) {
            Some(&id) => id,
            None => ensure_node(&mut tree, &mut path_map, parent_path),
        };
        tree.add_child(
            parent_id,
            FileNode {
                name,
                size: entry.size,
                is_dir: false,
                extension_id: ext_id,
                category: classification.category,
                parent: Some(parent_id),
                first_child: None,
                next_sibling: None,
                depth: 0,
            },
        );
    }
    aggregate::aggregate_sizes(&mut tree);
    aggregate::sort_children_by_size(&mut tree);
    tree
}

/// The directory node at `path`, creating it and any missing ancestors.
fn ensure_node(
    tree: &mut FileTree,
    path_map: &mut HashMap<PathBuf, NodeId>,
    path: &Path,
) -> NodeId {
    if let Some(&id) = path_map.get(path) {
        return id;
    }
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if path_map.contains_key(&current) {
            break;
        }
        missing.push(current.clone());
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent.to_path_buf(),
            _ => break,
        }
    }
    missing.reverse();
    let mut last = tree.root;
    for ancestor in missing {
        let parent_path = ancestor.parent().unwrap_or(Path::new("")).to_path_buf();
        let parent_id = path_map.get(&parent_path).copied().unwrap_or(tree.root);
        let name = ancestor
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let id = tree.add_child(
            parent_id,
            FileNode {
                name,
                size: 0,
                is_dir: true,
                extension_id: 0,
                category: FileCategory::Misc,
                parent: Some(parent_id),
                first_child: None,
                next_sibling: None,
                depth: 0,
            },
        );
        path_map.insert(ancestor, id);
        last = id;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_directory_bytes_are_kept_without_fake_leaf_rows_or_double_counting() {
        let entries = vec![
            RawEntry {
                path: "/root".into(),
                size: 5,
                is_dir: true,
            },
            RawEntry {
                path: "/root/cache".into(),
                size: 19,
                is_dir: true,
            },
            RawEntry {
                path: "/root/visible.txt".into(),
                size: 7,
                is_dir: false,
            },
        ];
        let mut tree = build_tree(&entries);
        assert_eq!(tree.get(tree.root).size, 31);
        assert_eq!(tree.nodes.len(), 3);
        assert_eq!(
            tree.nodes
                .iter()
                .find(|node| node.name == "cache")
                .unwrap()
                .size,
            19
        );
        aggregate::aggregate_sizes(&mut tree);
        assert_eq!(
            tree.get(tree.root).size,
            31,
            "reaggregation must preserve the cached base exactly once"
        );
    }

    #[test]
    fn entries_become_a_tree_with_aggregated_sizes_and_size_sorted_children() {
        let entries = vec![
            RawEntry {
                path: PathBuf::from("/r/a"),
                size: 0,
                is_dir: true,
            },
            RawEntry {
                path: PathBuf::from("/r/a/x.png"),
                size: 30,
                is_dir: false,
            },
            RawEntry {
                path: PathBuf::from("/r/b.txt"),
                size: 50,
                is_dir: false,
            },
            RawEntry {
                path: PathBuf::from("/r/a/deep/y.rs"),
                size: 5,
                is_dir: false,
            },
        ];
        let tree = build_tree(&entries);
        assert_eq!(tree.get(tree.root).size, 85);
        let children: Vec<&str> = tree
            .children(tree.root)
            .map(|id| tree.get(id).name.as_str())
            .collect();
        assert_eq!(children, vec!["b.txt", "a"], "sorted by size, descending");
        let a = tree.children(tree.root).nth(1).unwrap();
        assert_eq!(tree.get(a).size, 35);
        assert_eq!(
            tree.category_weights[tree.root.index()][2],
            30,
            "30 bytes of images"
        );
    }
}
