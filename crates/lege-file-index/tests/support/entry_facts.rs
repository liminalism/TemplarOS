//! One definition of "these two indexes hold the same rows", shared by every arm that compares a
//! delta against a fresh scan.
//!
//! It lives in `tests/support/` and is pulled into both consumers with `#[path]`: the privileged
//! Btrfs harness (`tests/btrfs_delta_fixture.rs`) and the crate's own no-Btrfs end-to-end tests
//! (`src/btrfs_delta.rs`). Two copies is exactly how the last bug survived three rounds of fixes —
//! the harness compared ten stored fields and the local tests compared paths, so a row that was in
//! the right place with the wrong timestamps only ever failed under `sudo` on the user's machine.
//! A comparison that is not the same comparison is not a reproduction.
//!
//! Cargo builds no test target from a subdirectory of `tests/`, so this file is compiled only
//! where it is included.
#![allow(dead_code)]

use std::{collections::BTreeMap, path::Path};

/// Every field the index stores that a scan can prove, keyed by path. This is the comparison the
/// lane lives or dies by: a delta that got any of these wrong would be silently corrupting rows.
pub type Fingerprint = BTreeMap<Vec<u8>, EntryFacts>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryFacts {
    pub kind: i64,
    pub size_bytes: i64,
    pub alloc_bytes: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: i64,
    pub hidden: i64,
    pub dev: Option<i64>,
    pub inode: Option<i64>,
    pub nlink: i64,
}

/// The live rows of an open index, as facts.
pub fn fingerprint_of(connection: &rusqlite::Connection) -> Fingerprint {
    let mut statement = connection
        .prepare(
            "SELECT path_bytes, kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden,
                    dev, inode, nlink
               FROM entries WHERE deleted_generation IS NULL ORDER BY path_bytes",
        )
        .expect("preparing the comparison query");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                EntryFacts {
                    kind: row.get(1)?,
                    size_bytes: row.get(2)?,
                    alloc_bytes: row.get(3)?,
                    mtime_ns: row.get(4)?,
                    ctime_ns: row.get(5)?,
                    mode: row.get(6)?,
                    hidden: row.get(7)?,
                    dev: row.get(8)?,
                    inode: row.get(9)?,
                    nlink: row.get(10)?,
                },
            ))
        })
        .expect("reading rows");
    rows.map(|row| row.expect("a row")).collect()
}

/// The same, for an index file nothing currently holds open.
pub fn fingerprint(database: &Path) -> Fingerprint {
    let connection =
        rusqlite::Connection::open(database).expect("opening the index for comparison");
    fingerprint_of(&connection)
}

/// What differs between two indexes, as text a person can act on.
pub fn describe_difference(delta: &Fingerprint, fresh: &Fingerprint) -> Option<String> {
    let mut problems = Vec::new();
    for (path, facts) in fresh {
        match delta.get(path) {
            None => problems.push(format!("missing from the delta index: {}", show(path))),
            Some(other) if other != facts => problems.push(format!(
                "differs at {}:\n    delta: {other:?}\n    fresh: {facts:?}",
                show(path)
            )),
            Some(_) => {}
        }
    }
    for path in delta.keys() {
        if !fresh.contains_key(path) {
            problems.push(format!("only in the delta index: {}", show(path)));
        }
    }
    if problems.is_empty() {
        return None;
    }
    let total = problems.len();
    problems.truncate(20);
    Some(format!(
        "{total} difference(s); first {}:\n  {}",
        problems.len(),
        problems.join("\n  ")
    ))
}

pub fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}
