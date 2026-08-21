//! Schema migrations, one module per owning domain.
//!
//! Applied in the order [`ORDER`] lists them, and recorded by name in `schema_migrations`.
//! Each domain owns exactly one file here so that work on different daemons never edits the
//! same list. Within a file the slice is **append-only**: once a migration has shipped, its
//! SQL is frozen and a change becomes a new entry.

pub mod files;
pub mod initial;
pub mod resources;
pub mod session;

/// Every migration, in application order.
pub fn all() -> Vec<(&'static str, &'static str)> {
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
