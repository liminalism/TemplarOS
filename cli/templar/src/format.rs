//! Generic, non-numeric rendering helpers shared by every subcommand's human-readable output.
//! Byte sizes, rates, durations and timestamps live in [`crate::humanize`]; this module is
//! everything else (currently just the column printer). Nothing here talks to a service: every
//! function is pure over already-fetched data, so it is testable with no session bus, no root
//! and no hardware — the same rule the subcommands themselves follow.

use std::fmt::Write as _;

/// A simple left-aligned column printer: pass rows of already-stringified cells, get back lines
/// with each column padded to its widest cell. No dependency, no unicode-width awareness beyond
/// `char` count (good enough for the ASCII-heavy identifiers this CLI prints).
pub fn table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index + 1 == columns || index + 1 == row.len() {
                out.push_str(cell);
            } else {
                let _ = write!(out, "{cell:<width$}  ", width = widths[index]);
            }
        }
        out.push('\n');
    }
    out.pop();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_pads_columns() {
        let rows = vec![
            vec!["a".to_owned(), "bb".to_owned()],
            vec!["ccc".to_owned(), "d".to_owned()],
        ];
        assert_eq!(table(&rows), "a    bb\nccc  d");
    }

    #[test]
    fn empty_table_is_empty() {
        assert_eq!(table(&[]), "");
    }
}
