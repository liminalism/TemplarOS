//! Migrations owned by `lege-resourced` (pressure traces reviewed before enforcement is
//! enabled). The governor never opens SQLite on its own loop (ADR-0002); `lege-sessiond`
//! ingests traces on its behalf.

use super::Migration;

pub const MIGRATIONS: &[Migration] = &[];
