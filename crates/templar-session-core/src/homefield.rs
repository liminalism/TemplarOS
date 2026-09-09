//! The Home Field's persistence: one `templar_field::FieldDocument` per output, edited only through
//! `templar_field::apply`, with every applied op's exact inverse kept in an undo log. The shell is a
//! restartable view and keeps no layout of its own (planning doc 01 §1, "the shell never owns
//! state"); this is where the layout lives, and `apply(inverse)` is the whole of undo.

use anyhow::{Context, Result, anyhow};
use rusqlite::{OptionalExtension, Transaction, params};
use templar_field::{FieldDocument, FieldOp, apply};
use templar_state::Store;

/// What was found in the store for one output.
#[derive(Debug)]
pub enum Stored {
    /// Nothing saved yet.
    Absent,
    /// A document this build can work with, brought forward if it needed it.
    Readable(Box<FieldDocument>),
    /// A document this build must not touch — written by a newer TemplarOS, or otherwise
    /// unreadable. **The stored copy stays exactly as it is.**
    Untouchable(templar_field::LoadError),
}

/// The stored document for `output`, migrated forward if an older build wrote it.
///
/// Returns [`Stored::Untouchable`] rather than an error for a document from the future, because
/// the caller's correct response is not to fail — the session still has to start — but to leave
/// the saved layout alone and carry on with an empty field.
pub fn load_stored(store: &Store, output: &str) -> Result<Stored> {
    let json: Option<String> = {
        let connection = store.lock();
        connection
            .query_row(
                "SELECT json FROM homefield WHERE output = ?1",
                params![output],
                |row| row.get(0),
            )
            .optional()
            .context("reading the home field document")?
    };
    let Some(json) = json else {
        return Ok(Stored::Absent);
    };
    Ok(match templar_field::load_document(&json) {
        Ok(doc) => Stored::Readable(Box::new(doc)),
        Err(error) => Stored::Untouchable(error),
    })
}

/// Every output that has a stored document, so a change that is about a tile rather than a
/// display (a garden place hidden or forgotten) can reach each field it is on.
pub fn outputs(store: &Store) -> Result<Vec<String>> {
    let connection = store.lock();
    let mut statement = connection
        .prepare("SELECT output FROM homefield ORDER BY output")
        .context("listing home field outputs")?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("listing home field outputs")?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .context("listing home field outputs")
}

/// The stored document for `output`, if any and if this build can read it.
pub fn load(store: &Store, output: &str) -> Result<Option<FieldDocument>> {
    Ok(match load_stored(store, output)? {
        Stored::Readable(doc) => Some(*doc),
        Stored::Absent | Stored::Untouchable(_) => None,
    })
}

/// The stored document, reshaped to `cols`×`rows` if the display it was saved on is not the one
/// asking, or a fresh empty one — persisted either way, so the first `Apply` has something to
/// apply to and `GetRevision` answers 0 rather than "no such output".
///
/// The reshape is what makes the stored layout belong to the *field* rather than to whichever
/// screen last drew it. Without it, a document saved at one resolution came back unchanged at
/// another and was drawn over the new pixel area regardless: cells the size of the old grid
/// stretched or squashed across the new one, and anything past a shrunken edge went out of reach.
/// `templar_field::reshape` re-seats every tile proportionally through the ordinary reflow, so the
/// result satisfies the same invariants a drop does.
pub fn load_or_create(store: &Store, output: &str, cols: u16, rows: u16) -> Result<FieldDocument> {
    match load_stored(store, output)? {
        Stored::Readable(doc) => {
            if doc.cols == cols && doc.rows == rows {
                return Ok(*doc);
            }
            let reshaped = templar_field::reshape(&doc, cols, rows);
            store.transaction(|tx| save_within(tx, &reshaped))?;
            Ok(reshaped)
        }
        // Nothing to read: an empty field, persisted, so the first `Apply` has something to
        // apply to and `GetRevision` answers 0 rather than "no such output".
        Stored::Absent => {
            let doc = FieldDocument::new(output, cols, rows);
            store.transaction(|tx| save_within(tx, &doc))?;
            Ok(doc)
        }
        // A document this build must not touch — a newer TemplarOS wrote it, which is what a rolled
        // back build set looks like from underneath. An empty field is returned so the session
        // still comes up, and it is deliberately **not** saved: writing it would replace the
        // arrangement with nothing and make the rollback permanent before anyone noticed it had
        // happened. Rolling forward again finds the layout where it was left.
        Stored::Untouchable(error) => {
            debug_assert!(error.leave_stored_document_alone());
            tracing::warn!(
                output,
                %error,
                "the saved Home Field was left untouched; showing an empty one for this session",
            );
            Ok(FieldDocument::new(output, cols, rows))
        }
    }
}

/// Discard `output`'s document and its whole undo log, and store a fresh empty one at
/// `cols`x`rows`.
///
/// The shell's next merge fills it with whatever the current default layout is, which is the only
/// way to adopt one: `field_model::merge` is add-only by design — it never moves a tile that
/// already has a place, so a field saved before a layout rule changed keeps the old arrangement
/// forever, and one the user has pushed into a mess can otherwise only be repaired tile by tile.
///
/// The undo log goes with it. Keeping it would leave inverses referring to placements on a
/// document that no longer exists, so this is deliberately not undoable, and `field.reset`
/// confirms before it is called.
pub fn reset(store: &Store, output: &str, cols: u16, rows: u16) -> Result<FieldDocument> {
    let doc = FieldDocument::new(output, cols, rows);
    store.transaction(|tx| {
        tx.execute(
            "DELETE FROM homefield_undo WHERE output = ?1",
            params![output],
        )
        .context("clearing the home field undo log")?;
        save_within(tx, &doc)
    })?;
    Ok(doc)
}

/// Insert or update the document (one JSON blob, keyed by output).
pub fn save_within(tx: &Transaction<'_>, doc: &FieldDocument) -> Result<()> {
    let json = serde_json::to_string(doc).context("serializing the home field")?;
    tx.execute(
        "INSERT INTO homefield (output, revision, json, updated_ms) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (output) DO UPDATE SET revision = excluded.revision, json = excluded.json,
         updated_ms = excluded.updated_ms",
        params![
            doc.output,
            doc.revision as i64,
            json,
            templar_state::now_ms()
        ],
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
        params![output, revision as i64, json, templar_state::now_ms()],
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
/// op does not apply, with `templar_field::FieldError` as the message.
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

/// Parse `op_json` as a `templar_field::FieldOp` and [`apply_op`] it.
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

    #[test]
    fn a_home_field_from_a_newer_build_is_left_exactly_as_it_is() {
        // What a rolled-back build set looks like from underneath. The older shell reads a
        // layout it does not understand; if it replaced it with an empty one and saved that, the
        // rollback would destroy the arrangement before anyone noticed it had happened.
        let store = Store::in_memory().unwrap();
        let mut value = serde_json::to_value(FieldDocument::new("DP-1", 8, 6)).unwrap();
        value["version"] = serde_json::Value::from(templar_field::DOCUMENT_VERSION + 1);
        let saved = serde_json::to_string(&value).unwrap();
        store
            .lock()
            .execute(
                "INSERT INTO homefield (output, revision, json, updated_ms) VALUES (?1, 7, ?2, 0)",
                params!["DP-1", saved],
            )
            .unwrap();

        assert!(matches!(
            load_stored(&store, "DP-1").unwrap(),
            Stored::Untouchable(_)
        ));

        // The session still comes up, with an empty field for this run.
        let doc = load_or_create(&store, "DP-1", 8, 6).unwrap();
        assert!(doc.tiles.is_empty());
        assert_eq!(doc.revision, 0);

        // And the stored bytes are byte-for-byte what they were.
        let after: String = store
            .lock()
            .query_row(
                "SELECT json FROM homefield WHERE output = ?1",
                params!["DP-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, saved, "the saved layout must survive being read");
    }

    #[test]
    fn a_current_document_still_round_trips_through_the_load_boundary() {
        let store = Store::in_memory().unwrap();
        let doc = load_or_create(&store, "DP-1", 8, 6).unwrap();
        assert_eq!(doc.version, templar_field::DOCUMENT_VERSION);
        assert!(matches!(
            load_stored(&store, "DP-1").unwrap(),
            Stored::Readable(_)
        ));
        assert!(matches!(
            load_stored(&store, "HDMI-1").unwrap(),
            Stored::Absent
        ));
    }
    use templar_field::{Cell, Landing, Span, SpanRange, Tile, TileId, TileKind};

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
