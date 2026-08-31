//! One sort contract for every file surface. `lege-filed` orders the listings it serves with
//! this module, and the Files app and the shell's Home folder surfaces re-order fetched
//! listings with the *same* comparator — so the same directory never silently changes order
//! between surfaces, which is the whole reason this lives in the shared types crate rather
//! than in any one of them.
//!
//! Every ordering here is a deterministic total order: the primary key comes from the
//! descriptor, direction flips only that primary key, and the tie breakers — natural name,
//! exact name, then path — always run ascending, so reversing a sort never shuffles equal
//! items.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::Path;

/// How a folder or pane surface presents its entries. Shared for the same reason as the sort
/// descriptor: Home and Files must mean the same thing by "thumbnails".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewMode {
    /// Name, type, size and modified time in bounded columns.
    #[default]
    List,
    /// Responsive preview cells, loaded boundedly off the render thread.
    Thumbnails,
}

/// What the primary key is.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortField {
    /// Case-folded natural order: `file2` before `file10`.
    #[default]
    NaturalName,
    /// The entry's kind label (an extension, or a surface-chosen type name), then name.
    Kind,
    Size,
    Modified,
    /// A shortcut pane's own reference order. Items without a manual index sort after those
    /// with one; surfaces over plain directories fall back to natural name.
    Manual,
}

/// One surface's declared ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SortDescriptor {
    pub field: SortField,
    pub ascending: bool,
    /// Directories before files, regardless of direction. The explicit folder-first policy —
    /// off means folders interleave by the primary key like anything else.
    pub folders_first: bool,
}

impl Default for SortDescriptor {
    /// The order `lege-filed` has always served: folders first, natural name, ascending.
    fn default() -> Self {
        Self {
            field: SortField::NaturalName,
            ascending: true,
            folders_first: true,
        }
    }
}

/// The facts the comparator reads about one entry. Borrowed, so every surface adapts its own
/// entry type without copying listings around.
#[derive(Clone, Copy, Debug)]
pub struct SortItem<'a> {
    pub name: &'a str,
    pub path: &'a Path,
    pub is_dir: bool,
    /// Hidden entries group after visible ones within their folder/file band.
    pub hidden: bool,
    /// The label [`SortField::Kind`] compares — an extension, or a type name. Empty is fine.
    pub kind_label: &'a str,
    pub size_bytes: u64,
    pub mtime_ms: i64,
    /// The entry's position in a shortcut pane's manual order, when it has one.
    pub manual_index: Option<u64>,
}

/// The one comparator. Total and deterministic for any two items with distinct paths.
#[must_use]
pub fn compare(a: &SortItem<'_>, b: &SortItem<'_>, descriptor: SortDescriptor) -> Ordering {
    if descriptor.folders_first {
        let grouped = b.is_dir.cmp(&a.is_dir);
        if grouped != Ordering::Equal {
            return grouped;
        }
    }
    let hidden = a.hidden.cmp(&b.hidden);
    if hidden != Ordering::Equal {
        return hidden;
    }
    let primary = match descriptor.field {
        SortField::NaturalName => natural_cmp(a.name, b.name),
        SortField::Kind => natural_cmp(a.kind_label, b.kind_label),
        SortField::Size => a.size_bytes.cmp(&b.size_bytes),
        SortField::Modified => a.mtime_ms.cmp(&b.mtime_ms),
        SortField::Manual => match (a.manual_index, b.manual_index) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => natural_cmp(a.name, b.name),
        },
    };
    let primary = if descriptor.ascending {
        primary
    } else {
        primary.reverse()
    };
    primary
        .then_with(|| natural_cmp(a.name, b.name))
        .then_with(|| a.name.cmp(b.name))
        .then_with(|| a.path.cmp(b.path))
}

/// Deterministic, locale-neutral natural comparison. Case-folded non-numeric text is compared
/// bytewise; adjacent ASCII digit runs compare by numeric magnitude without integer overflow.
#[must_use]
pub fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    let (a, b) = (left.as_bytes(), right.as_bytes());
    let (mut ai, mut bi) = (0, 0);
    while ai < a.len() && bi < b.len() {
        if a[ai].is_ascii_digit() && b[bi].is_ascii_digit() {
            let a_end = digit_end(a, ai);
            let b_end = digit_end(b, bi);
            let a_significant = significant_digits(&a[ai..a_end]);
            let b_significant = significant_digits(&b[bi..b_end]);
            let order = a_significant
                .len()
                .cmp(&b_significant.len())
                .then_with(|| a_significant.cmp(b_significant))
                .then_with(|| (a_end - ai).cmp(&(b_end - bi)));
            if order != Ordering::Equal {
                return order;
            }
            ai = a_end;
            bi = b_end;
        } else {
            let order = a[ai].cmp(&b[bi]);
            if order != Ordering::Equal {
                return order;
            }
            ai += 1;
            bi += 1;
        }
    }
    a.len().cmp(&b.len())
}

fn digit_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map_or(bytes.len(), |offset| start + offset)
}

fn significant_digits(run: &[u8]) -> &[u8] {
    let first = run.iter().position(|byte| *byte != b'0');
    first.map_or(&run[run.len()..], |first| &run[first..])
}
