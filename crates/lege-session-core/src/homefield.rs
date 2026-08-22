//! The Home Field's persistence: one `lege_field::FieldDocument` per output, edited only through
//! `lege_field::apply`, with every applied op's exact inverse kept in an undo log. The shell is a
//! restartable view and keeps no layout of its own (planning doc 01 §1, "the shell never owns
//! state"); this is where the layout lives, and `apply(inverse)` is the whole of undo.

use anyhow::{Context, Result, anyhow};
use lege_field::{FieldDocument, FieldOp, apply};
use lege_state::Store;
use rusqlite::{OptionalExtension, Transaction, params};

/// The stored document for `output`, if any.
pub fn load(store: &Store, output: &str) -> Result<Option<FieldDocument>> {
    let connection = store.lock();
    let json: Option<String> = connection
        .query_row(
            "SELECT json FROM homefield WHERE output = ?1",
            params![output],
            |row| row.get(0),
        )
        .optional()
        .context("reading the home field document")?;
    json.map(|json| serde_json::from_str(&json).context("parsing the stored home field"))
        .transpose()
}

/// The stored document, or a fresh empty one at `cols`×`rows` — persisted, so the first `Apply`
/// has something to apply to and `GetRevision` answers 0 rather than "no such output".
pub fn load_or_create(store: &Store, output: &str, cols: u16, rows: u16) -> Result<FieldDocument> {
    if let Some(doc) = load(store, output)? {
        return Ok(doc);
    }
    let doc = FieldDocument::new(output, cols, rows);
    store.transaction(|tx| save_within(tx, &doc))?;
    Ok(doc)
}

/// Insert or update the document (one JSON blob, keyed by output).
pub fn save_within(tx: &Transaction<'_>, doc: &FieldDocument) -> Result<()> {
    let json = serde_json::to_string(doc).context("serializing the home field")?;
    tx.execute(
        "INSERT INTO homefield (output, revision, json, updated_ms) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (output) DO UPDATE SET revision = excluded.revision, json = excluded.json,
         updated_ms = excluded.updated_ms",
        params![doc.output, doc.revision as i64, json, lege_state::now_ms()],
    )
    .context("persisting the home field")?;
    Ok(())
}

fn push_undo_within(
    tx: &Transaction<'_>,
    output: &str,
    revision: u64,
    inverse: &FieldOp,
) -> Result<()> {
    let json = serde_json::to_string(inverse).context("serializing the inverse op")?;
    tx.execute(
        "INSERT INTO homefield_undo (output, revision, inverse_json, at_ms) VALUES (?1, ?2, ?3, ?4)",
        params![output, revision as i64, json, lege_state::now_ms()],
    )
    .context("recording the inverse op")?;
    Ok(())
}

fn pop_undo_within(tx: &Transaction<'_>, output: &str) -> Result<Option<FieldOp>> {
    let row: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, inverse_json FROM homefield_undo WHERE output = ?1 ORDER BY id DESC LIMIT 1",
            params![output],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("reading the undo log")?;
    let Some((id, json)) = row else {
        return Ok(None);
    };
    tx.execute("DELETE FROM homefield_undo WHERE id = ?1", params![id])
        .context("consuming the undo entry")?;
    Ok(Some(
        serde_json::from_str(&json).context("parsing the inverse op")?,
    ))
}

/// How many undo entries an output keeps; older ones are dropped on the next apply.
const UNDO_DEPTH: i64 = 100;

/// Apply `op` to `output`'s stored document, bump its revision, and keep the inverse. The document
/// must exist (`load_or_create`). Returns the new document. Refuses — storing nothing — when the
/// op does not apply, with `lege_field::FieldError` as the message.
pub fn apply_op(store: &Store, output: &str, op: &FieldOp) -> Result<FieldDocument> {
    let doc = load(store, output)?.ok_or_else(|| anyhow!("no home field stored for {output:?}"))?;
    let (mut next, inverse) = apply(&doc, op).map_err(|error| anyhow!("{error}"))?;
    next.revision = doc.revision + 1;
    let revision = next.revision;
    store.transaction(|tx| {
        save_within(tx, &next)?;
        push_undo_within(tx, output, revision, &inverse)?;
        tx.execute(
            "DELETE FROM homefield_undo WHERE output = ?1 AND id NOT IN
             (SELECT id FROM homefield_undo WHERE output = ?1 ORDER BY id DESC LIMIT ?2)",
            params![output, UNDO_DEPTH],
        )
        .context("trimming the undo log")?;
        Ok(())
    })?;
    Ok(next)
}

/// Parse `op_json` as a `lege_field::FieldOp` and [`apply_op`] it.
pub fn apply_json(store: &Store, output: &str, op_json: &str) -> Result<FieldDocument> {
    let op: FieldOp = serde_json::from_str(op_json).context("parsing the field op")?;
    apply_op(store, output, &op)
}

/// Apply the inverse of the most recent op. `None` when there is nothing to undo.
pub fn undo(store: &Store, output: &str) -> Result<Option<FieldDocument>> {
    let doc = load(store, output)?.ok_or_else(|| anyhow!("no home field stored for {output:?}"))?;
    store.transaction(|tx| {
        let Some(inverse) = pop_undo_within(tx, output)? else {
            return Ok(None);
        };
        let (mut next, _) = apply(&doc, &inverse).map_err(|error| anyhow!("{error}"))?;
        next.revision = doc.revision + 1;
        save_within(tx, &next)?;
        Ok(Some(next))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_field::{Cell, Landing, Span, SpanRange, Tile, TileId, TileKind};

    fn tile(id: u64) -> Tile {
        Tile {
            id: TileId(id),
            kind: TileKind::System {
                probe: "drives".into(),
            },
            title: format!("t{id}"),
            spans: SpanRange::fixed(Span::new(1, 1)),
            accent: None,
        }
    }

    fn place(id: u64, col: u16) -> FieldOp {
        FieldOp::Place {
            tile: tile(id),
            landing: Landing {
                container: None,
                cell: Cell::new(col, 0),
            },
        }
    }

    #[test]
    fn apply_bumps_the_revision_and_undo_restores_in_one_transaction_each() {
        let store = Store::in_memory().unwrap();
        assert_eq!(load(&store, "eDP-1").unwrap(), None);
        let empty = load_or_create(&store, "eDP-1", 8, 6).unwrap();
        assert_eq!(empty.revision, 0);
        assert_eq!(load(&store, "eDP-1").unwrap(), Some(empty.clone()));

        let one = apply_op(&store, "eDP-1", &place(1, 0)).unwrap();
        assert_eq!(one.revision, 1);
        let two = apply_op(&store, "eDP-1", &place(2, 3)).unwrap();
        assert_eq!(two.revision, 2);
        assert_eq!(load(&store, "eDP-1").unwrap(), Some(two.clone()));

        let back = undo(&store, "eDP-1").unwrap().unwrap();
        assert_eq!(back.revision, 3);
        assert_eq!(back.located, one.located, "the second place was undone");
        let back = undo(&store, "eDP-1").unwrap().unwrap();
        assert_eq!(back.located, empty.located);
        assert_eq!(undo(&store, "eDP-1").unwrap(), None, "nothing left to undo");
    }

    #[test]
    fn a_refused_op_stores_nothing() {
        let store = Store::in_memory().unwrap();
        load_or_create(&store, "eDP-1", 2, 1).unwrap();
        apply_op(&store, "eDP-1", &place(1, 0)).unwrap();
        let refused = apply_op(&store, "eDP-1", &place(1, 1));
        assert!(refused.unwrap_err().to_string().contains("already exists"));
        assert_eq!(load(&store, "eDP-1").unwrap().unwrap().revision, 1);
        assert!(undo(&store, "eDP-1").unwrap().is_some());
        assert!(
            undo(&store, "eDP-1").unwrap().is_none(),
            "the refused op left no undo entry"
        );
    }

    #[test]
    fn ops_arrive_as_json_and_outputs_are_independent() {
        let store = Store::in_memory().unwrap();
        load_or_create(&store, "a", 4, 4).unwrap();
        load_or_create(&store, "b", 4, 4).unwrap();
        let json = serde_json::to_string(&place(1, 0)).unwrap();
        let a = apply_json(&store, "a", &json).unwrap();
        assert_eq!(a.tiles.len(), 1);
        assert_eq!(load(&store, "b").unwrap().unwrap().tiles.len(), 0);
        assert!(apply_json(&store, "a", "not json").is_err());
        assert!(apply_json(&store, "missing", &json).is_err());
    }
}
