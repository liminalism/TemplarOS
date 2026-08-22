//! A flat arena of nodes with sibling lists: `u32` ids, children prepended in O(1), and every
//! node after its parent — which is what lets aggregation run as one reverse pass.

use std::collections::HashMap;
use std::path::PathBuf;

use super::extensions::{CATEGORY_COUNT, FileCategory, category_index};

/// Index into the arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// One file or directory.
#[derive(Debug, Clone)]
pub struct FileNode {
    pub name: String,
    /// Bytes: a file's own size, a directory's aggregated total.
    pub size: u64,
    pub is_dir: bool,
    /// Index into [`FileTree::extensions`] (0 = none).
    pub extension_id: u16,
    pub category: FileCategory,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub depth: u16,
}

/// The tree.
#[derive(Debug)]
pub struct FileTree {
    pub nodes: Vec<FileNode>,
    pub root: NodeId,
    pub root_path: PathBuf,
    /// Interned, lower-cased extensions; index 0 is "no extension".
    pub extensions: Vec<String>,
    extension_ids: HashMap<String, u16>,
    /// Bytes per category under each node (a directory's composition).
    pub category_weights: Vec<[u64; CATEGORY_COUNT]>,
}

impl FileTree {
    pub fn new(root_name: &str, root_path: PathBuf) -> Self {
        let root = FileNode {
            name: root_name.to_owned(),
            size: 0,
            is_dir: true,
            extension_id: 0,
            category: FileCategory::Misc,
            parent: None,
            first_child: None,
            next_sibling: None,
            depth: 0,
        };
        Self {
            nodes: vec![root],
            root: NodeId(0),
            root_path,
            extensions: vec![String::new()],
            extension_ids: HashMap::from([(String::new(), 0)]),
            category_weights: vec![[0; CATEGORY_COUNT]],
        }
    }

    /// Add a child under `parent` (prepended to its sibling list); returns its id.
    pub fn add_child(&mut self, parent: NodeId, mut node: FileNode) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        let size = node.size;
        let is_dir = node.is_dir;
        let category = node.category;
        node.parent = Some(parent);
        node.depth = self.nodes[parent.index()].depth + 1;
        node.next_sibling = self.nodes[parent.index()].first_child;
        self.nodes[parent.index()].first_child = Some(id);
        self.nodes.push(node);
        let mut weights = [0; CATEGORY_COUNT];
        if !is_dir && size > 0 {
            weights[category_index(category)] = size;
        }
        self.category_weights.push(weights);
        id
    }

    pub fn get(&self, id: NodeId) -> &FileNode {
        &self.nodes[id.index()]
    }

    pub fn get_mut(&mut self, id: NodeId) -> &mut FileNode {
        &mut self.nodes[id.index()]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Only the root.
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub fn children(&self, parent: NodeId) -> ChildIter<'_> {
        ChildIter {
            tree: self,
            current: self.nodes[parent.index()].first_child,
        }
    }

    /// The id for `ext` (lower-cased), interning it on first sight.
    pub fn intern_extension(&mut self, ext: &str) -> u16 {
        let lower = ext.to_ascii_lowercase();
        if let Some(&id) = self.extension_ids.get(&lower) {
            return id;
        }
        let id = self.extensions.len() as u16;
        self.extensions.push(lower.clone());
        self.extension_ids.insert(lower, id);
        id
    }
}

/// Children of one node, in sibling-list order.
#[derive(Debug)]
pub struct ChildIter<'a> {
    tree: &'a FileTree,
    current: Option<NodeId>,
}

impl Iterator for ChildIter<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let id = self.current?;
        self.current = self.tree.nodes[id.index()].next_sibling;
        Some(id)
    }
}
