//! Schema migrations, one module per owning domain.
//!
//! Applied in order and recorded by name in `schema_migrations`. Each domain owns exactly one
//! file here so that work on different daemons never edits the same list. Within a file the slice
//! is **append-only**: once a migration has shipped, its SQL is frozen and a change becomes a new
//! entry.
//!
//! ## Backward readability
//!
//! Migrations here have always been forward-only, with no statement anywhere about whether an
//! *older* binary could still read a database a newer one had migrated. That question is not
//! academic: rolling a build set back installs the previous binaries against the database the
//! newer ones already migrated, and if that database is unreadable the rollback has traded a
//! broken shell for a broken session.
//!
//! So each migration declares it. [`Migration::additive`] — the overwhelming majority — adds
//! tables or columns that an older reader simply never selects, and raises no floor.
//! [`Migration::breaking`] changes or removes something an older reader depends on, and names the
//! schema version a reader must have reached to cope. The effective floor is the highest such
//! value among applied migrations, recorded in `schema_meta`, and [`crate::open`] refuses a
//! database whose floor is above what this build understands rather than corrupting it.

pub mod files;
pub mod initial;
pub mod resources;
pub mod session;

/// One schema migration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Migration {
    /// Ordered, unique, never reused. Recorded in `schema_migrations`.
    pub name: &'static str,
    /// The SQL, frozen once shipped.
    pub sql: &'static str,
    /// The schema version a reader must understand to read a database this has been applied to.
    ///
    /// `0` means it raises no floor: an older binary that has never heard of this migration can
    /// still read the database, because everything it does is additive. Anything else is a
    /// promise being withdrawn, and must be the migration's own schema version.
    pub min_reader_version: u32,
}

impl Migration {
    /// A migration an older reader can ignore: new tables, new columns, new indexes.
    #[must_use]
    pub const fn additive(name: &'static str, sql: &'static str) -> Self {
        Self {
            name,
            sql,
            min_reader_version: 0,
        }
    }

    /// A migration that changes or removes something an older reader depends on.
    ///
    /// `min_reader_version` is the schema version from which a binary can still read the result —
    /// in practice this migration's own position in the order. Shipping one of these is what
    /// makes a rollback past it impossible, so it also has to ship with a pre-upgrade snapshot
    /// ([`crate::snapshot_to`]); see the policy tests.
    #[must_use]
    pub const fn breaking(name: &'static str, sql: &'static str, min_reader_version: u32) -> Self {
        Self {
            name,
            sql,
            min_reader_version,
        }
    }
}

/// Every migration, in application order.
#[must_use]
pub fn all() -> Vec<Migration> {
    let mut migrations = Vec::new();
    for group in [
        initial::MIGRATIONS,
        session::MIGRATIONS,
        files::MIGRATIONS,
        resources::MIGRATIONS,
    ] {
        migrations.extend_from_slice(group);
    }
    migrations
}
