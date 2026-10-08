//! Reading a persisted Home Field, including one an older build wrote.
//!
//! `DOCUMENT_VERSION` has always been checked by equality: a document from any other version was
//! refused. That is right for the *in-memory* guard in [`crate::ops::apply`] — applying an
//! operation to a document this code does not understand is how a layout gets silently mangled —
//! and wrong at the load boundary, where the two directions are not the same problem at all.
//!
//! **Older** is a problem this crate can solve: it knows what changed, so it can bring the
//! document forward. [`DOCUMENT_MIGRATIONS`] is where those steps go. Version 2 gives the
//! formerly ordinary Desktop folder its independent dock kind, retaining its tile identity
//! and every location. Resolving the current XDG path remains a merge-time responsibility.
//!
//! **Newer** is a problem this crate cannot solve, and the only question is what it does about
//! it. A document written by a build from the future is refused, the caller is told, and
//! **nothing is written back**. That last part is the whole of it: rolling a build set back and
//! logging in would otherwise have the older shell read a layout it did not understand, replace
//! it with an empty one, and save that — destroying the arrangement before anyone noticed the
//! rollback had happened. A person who rolls forward again should find their Home Field where
//! they left it.

use crate::{DOCUMENT_VERSION, field::FieldDocument};
use std::fmt;

/// One step from a document version to the next.
///
/// Migrations are *add-only and id-preserving*, the same rule the shell's merge-time migrations
/// follow: a step may add a field, give an absent one a default, or rewrite a representation, and
/// may not invent or renumber tiles. A layout is spatial memory, and a migration that moved
/// things would be indistinguishable from the user's own arrangement having been lost.
#[derive(Clone, Copy)]
pub struct DocumentMigration {
    /// The version this step reads.
    pub from: u32,
    /// What it reads it as. Always `from + 1`; the registry asserts it.
    pub to: u32,
    /// A short description, for the log line a person sees once.
    pub summary: &'static str,
    /// The step itself, over the parsed JSON — deliberately not over `FieldDocument`, which is
    /// only ever the *current* shape and cannot represent an older one.
    pub apply: fn(&mut serde_json::Value),
}

impl fmt::Debug for DocumentMigration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentMigration")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

/// Every step from an older document version to the current one, in order.
///
/// The first versioned step introduces the independent Desktop dock. Only the historic
/// named Desktop tile is recognised here: arbitrary folders titled Desktop are not docks.
pub const DOCUMENT_MIGRATIONS: &[DocumentMigration] = &[DocumentMigration {
    from: 1,
    to: 2,
    summary: "Independent Desktop dock, retaining placement",
    apply: migrate_desktop_dock,
}];

fn migrate_desktop_dock(value: &mut serde_json::Value) {
    let Some(tiles) = value.get_mut("tiles").and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for tile in tiles {
        if tile.get("id").and_then(serde_json::Value::as_u64)
            == Some(crate::TileId::for_key("folder:desktop").0)
            && tile.pointer("/kind/kind").and_then(serde_json::Value::as_str) == Some("folder")
        {
            tile["kind"]["kind"] = "desktop-dock".into();
            tile["kind"]["order"] = serde_json::json!([]);
        }
    }
}

/// Why a persisted document could not be read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// The JSON is not a Home Field document.
    Malformed(String),
    /// Written by a newer build. **The stored document has not been touched.**
    TooNew { found: u32, understood: u32 },
    /// Older than anything this build knows how to bring forward — a gap in the registry.
    NoMigration { found: u32, stuck_at: u32 },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "not a Home Field document: {detail}"),
            Self::TooNew { found, understood } => write!(
                f,
                "this Home Field was saved by a newer TemplarOS (document version {found}; this one \
                 reads {understood}). It has been left exactly as it is — install the newer build \
                 again and your layout will be there."
            ),
            Self::NoMigration { found, stuck_at } => write!(
                f,
                "this Home Field is document version {found} and there is no step from version \
                 {stuck_at} to the next; it has been left exactly as it is"
            ),
        }
    }
}

impl std::error::Error for LoadError {}

impl LoadError {
    /// Whether the caller should leave the stored document alone.
    ///
    /// True for every variant. It is a method rather than a comment because the one thing a
    /// caller must not do on any of these paths is write something else over the file.
    #[must_use]
    pub fn leave_stored_document_alone(&self) -> bool {
        true
    }
}

/// Reads a persisted document, migrating it forward if it was written by an older build.
///
/// # Errors
/// [`LoadError::TooNew`] for a document from a newer build, [`LoadError::NoMigration`] for a gap
/// in the registry, [`LoadError::Malformed`] for anything that is not a document. In every case
/// the caller must leave the stored copy untouched.
pub fn load_document(json: &str) -> Result<FieldDocument, LoadError> {
    let mut value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| LoadError::Malformed(error.to_string()))?;
    let found = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| LoadError::Malformed("no version field".to_owned()))?;

    if found > DOCUMENT_VERSION {
        return Err(LoadError::TooNew {
            found,
            understood: DOCUMENT_VERSION,
        });
    }

    let mut at = found;
    while at < DOCUMENT_VERSION {
        let step = DOCUMENT_MIGRATIONS
            .iter()
            .find(|migration| migration.from == at)
            .ok_or(LoadError::NoMigration {
                found,
                stuck_at: at,
            })?;
        (step.apply)(&mut value);
        value["version"] = serde_json::Value::from(step.to);
        at = step.to;
    }

    serde_json::from_value(value).map_err(|error| LoadError::Malformed(error.to_string()))
}

/// The migrations that would run for a document at `version`, for a caller that wants to say so
/// before doing it.
#[must_use]
pub fn migrations_from(version: u32) -> Vec<&'static str> {
    let mut summaries = Vec::new();
    let mut at = version;
    while at < DOCUMENT_VERSION {
        let Some(step) = DOCUMENT_MIGRATIONS
            .iter()
            .find(|migration| migration.from == at)
        else {
            break;
        };
        summaries.push(step.summary);
        at = step.to;
    }
    summaries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::FieldDocument;

    fn document() -> FieldDocument {
        FieldDocument::new("DP-1", 8, 6)
    }

    #[test]
    fn a_current_document_loads_unchanged() {
        let doc = document();
        let json = serde_json::to_string(&doc).unwrap();
        assert_eq!(load_document(&json).unwrap(), doc);
    }

    #[test]
    fn a_document_from_the_future_is_refused_and_says_what_to_do() {
        // The rollback case, and the reason this exists at all: an older shell reading a newer
        // layout must not replace it with an empty one and save that.
        let mut value = serde_json::to_value(document()).unwrap();
        value["version"] = serde_json::Value::from(DOCUMENT_VERSION + 1);
        let error = load_document(&serde_json::to_string(&value).unwrap()).unwrap_err();
        assert_eq!(
            error,
            LoadError::TooNew {
                found: DOCUMENT_VERSION + 1,
                understood: DOCUMENT_VERSION,
            }
        );
        assert!(error.leave_stored_document_alone());
        let text = error.to_string();
        assert!(text.contains("left exactly as it is"), "{text}");
        assert!(text.contains("your layout will be there"), "{text}");
    }

    #[test]
    fn an_unmigratable_older_document_is_refused_rather_than_guessed_at() {
        let mut value = serde_json::to_value(document()).unwrap();
        value["version"] = serde_json::Value::from(0_u32);
        let error = load_document(&serde_json::to_string(&value).unwrap()).unwrap_err();
        assert_eq!(
            error,
            LoadError::NoMigration {
                found: 0,
                stuck_at: 0
            }
        );
        assert!(error.leave_stored_document_alone());
    }

    #[test]
    fn something_that_is_not_a_document_is_malformed_not_too_new() {
        assert!(matches!(load_document("{"), Err(LoadError::Malformed(_))));
        assert!(matches!(load_document("{}"), Err(LoadError::Malformed(_))));
        assert!(matches!(
            load_document(r#"{"version":"one"}"#),
            Err(LoadError::Malformed(_))
        ));
    }

    #[test]
    fn the_registry_is_a_chain_with_no_gaps_and_no_forks() {
        // Every registered migration must be a one-step, unambiguous forward chain.
        let mut seen = std::collections::BTreeSet::new();
        for migration in DOCUMENT_MIGRATIONS {
            assert_eq!(
                migration.to,
                migration.from + 1,
                "a migration goes one version forward"
            );
            assert!(
                seen.insert(migration.from),
                "two migrations read version {}",
                migration.from
            );
            assert!(
                migration.to <= DOCUMENT_VERSION,
                "{} is beyond the version this build writes",
                migration.to
            );
            assert!(
                !migration.summary.is_empty(),
                "a migration says what it does"
            );
        }
        // Every version from the oldest the registry handles up to the current one is reachable.
        if let Some(oldest) = DOCUMENT_MIGRATIONS.iter().map(|m| m.from).min() {
            assert_eq!(
                migrations_from(oldest).len(),
                (DOCUMENT_VERSION - oldest) as usize
            );
        }
    }

    #[test]
    fn current_version_has_no_pending_migrations() {
        assert!(migrations_from(DOCUMENT_VERSION).is_empty());
        assert_eq!(migrations_from(1), vec!["Independent Desktop dock, retaining placement"]);
    }

    #[test]
    fn version_one_desktop_cutover_preserves_id_geometry_and_other_folders() {
        use crate::{Cell, Located, Placement, Span, TileId, TileKind, folder_tile};
        let mut doc = document();
        let id = TileId::for_key("folder:desktop");
        let at = Located { container: None, placement: Placement::new(Cell::new(1, 2), Span::new(3, 3)) };
        doc.tiles.push(folder_tile(id, std::path::Path::new("/home/a/Bureau"), 8, 6));
        let other = TileId(4);
        doc.tiles.push(folder_tile(other, std::path::Path::new("/work/Desktop"), 8, 6));
        doc.tiles.sort_by_key(|tile| tile.id);
        doc.located.push((id, at));
        doc.version = 1;
        let migrated = load_document(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(migrated.version, DOCUMENT_VERSION);
        assert_eq!(migrated.location(id), Some(at));
        assert!(matches!(&migrated.tile(id).unwrap().kind, TileKind::DesktopDock { path, order }
            if path == std::path::Path::new("/home/a/Bureau") && order.is_empty()));
        assert!(matches!(&migrated.tile(other).unwrap().kind, TileKind::Folder { .. }));
        assert_eq!(migrated.tiles.len(), doc.tiles.len());
        assert_eq!(migrated.revision, doc.revision);
    }
}
