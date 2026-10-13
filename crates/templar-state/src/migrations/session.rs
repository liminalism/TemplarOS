//! Migrations owned by `templar-sessiond` (identity, associations, providers, autostart, history,
//! notifications, settings, projects, power, location).
//!
//! `0001-initial` (in [`super::initial`]) already carries `instances`, `mime_defaults`,
//! `extension_defaults`, `project_mime_defaults`, `provider_selections`, `autostart_approvals`,
//! `history`, `projects` and `settings_changes`. This slice adds what M1 needs on top: current
//! settings *values* (as opposed to their change log), provider *declarations* (as opposed to
//! the single selection per role), a small notification-ID ledger so a fast crash/restart never
//! reissues a live ID, one shared undo log for every domain that offers "Undo", desktop-entry
//! import bookkeeping (so re-scans are incremental and a later native manifest can be detected),
//! and the `systemd-coredump` journal cursor. `0003` adds persisted background-permission
//! declarations, so `org.templaros.Background1` can answer "what does this app want and why is it
//! running" without a fresh `.desktop` scan. `0006` records what every `app_id` here has always
//! silently been — one *variant*, one installed copy from one origin — and the logical
//! application it is a copy of.

use super::Migration;

pub const MIGRATIONS: &[Migration] = &[
    Migration::additive(
        "0002-session-domains",
        r"
        CREATE TABLE settings_values (
            key        TEXT PRIMARY KEY,
            value_json TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );
        CREATE TABLE provider_declarations (
            app_id   TEXT NOT NULL,
            role     TEXT NOT NULL,
            priority INTEGER NOT NULL,
            PRIMARY KEY (app_id, role)
        );
        CREATE TABLE notifications_active (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id   TEXT NOT NULL,
            app_name   TEXT NOT NULL,
            app_id     TEXT,
            created_ms INTEGER NOT NULL
        );
        -- One shared undo log for every domain that can reverse a change (associations,
        -- settings, provider selection). `domain` picks the applier; `key` is the row identity
        -- inside that domain (a MIME type, an extension, `<project_id>\x1f<mime>`, a setting key
        -- or a provider role). `event_id` is the history event whose 'Undo' action names this
        -- row (`undo:<id>`).
        CREATE TABLE change_log (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            at_ms     INTEGER NOT NULL,
            domain    TEXT NOT NULL,
            key       TEXT NOT NULL,
            old_value TEXT,
            new_value TEXT,
            event_id  TEXT NOT NULL,
            undone    INTEGER NOT NULL DEFAULT 0
        );
        -- One row per imported .desktop file, so a re-scan is incremental (mtime-keyed) and a
        -- later-installed native manifest can be detected and preferred.
        CREATE TABLE desktop_import (
            desktop_id  TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            mtime_ms    INTEGER NOT NULL,
            app_id      TEXT NOT NULL,
            imported_ms INTEGER NOT NULL
        );
        -- systemd-coredump journal ingestion progress (ADR-0011): the journal entry cursor of
        -- the last crash event turned into history, so a restart never re-ingests or misses one.
        CREATE TABLE coredump_cursor (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            cursor     TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );
",
    ),
    Migration::additive(
        "0003-background-declarations",
        r"
        -- What the last autostart scan found this application declaring (planning doc 01 §7):
        -- the background permissions it requests and, if any, the exclusive role it ties its
        -- autostart to. Separate from `autostart_approvals` (the user's yes/no on top of it) so
        -- org.templaros.Background1 can answer 'what does it want, and did we say yes' without a
        -- fresh .desktop scan.
        CREATE TABLE background_declarations (
            app_id    TEXT PRIMARY KEY,
            role      TEXT,
            requested TEXT NOT NULL
        );
",
    ),
    Migration::additive(
        "0004-homefield",
        r"
        -- The Home Field: one document per output (`templar_field::FieldDocument`, one JSON blob,
        -- edited as a unit through `templar_field::apply`), and its undo log — the exact inverse of
        -- every applied op, most recent last, so `HomeField1.Undo` is `apply(inverse)` and
        -- nothing above this table keeps a second undo model.
        CREATE TABLE homefield (
            output     TEXT PRIMARY KEY,
            revision   INTEGER NOT NULL,
            json       TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );
        CREATE TABLE homefield_undo (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            output       TEXT NOT NULL,
            revision     INTEGER NOT NULL,
            inverse_json TEXT NOT NULL,
            at_ms        INTEGER NOT NULL
        );
        CREATE INDEX homefield_undo_output ON homefield_undo (output, id);
",
    ),
    Migration::additive(
        "0005-notification-details",
        r"
        -- `notifications_active` used to be only an ID ledger.  The notification server is the
        -- policy owner, so the shell needs a complete read model here instead of reconstructing
        -- presentation state from History1's human-oriented sentences.
        ALTER TABLE notifications_active ADD COLUMN summary TEXT NOT NULL DEFAULT '';
        ALTER TABLE notifications_active ADD COLUMN body TEXT NOT NULL DEFAULT '';
        ALTER TABLE notifications_active ADD COLUMN icon TEXT NOT NULL DEFAULT '';
        ALTER TABLE notifications_active ADD COLUMN urgency INTEGER NOT NULL DEFAULT 1;
        ALTER TABLE notifications_active ADD COLUMN category TEXT NOT NULL DEFAULT '';
        ALTER TABLE notifications_active ADD COLUMN actions_json TEXT NOT NULL DEFAULT '[]';
        ALTER TABLE notifications_active ADD COLUMN expires_ms INTEGER;
",
    ),
    Migration::additive(
        "0006-app-variants",
        r"
        -- Every app_id already in this database names one *variant*: one installed copy of an
        -- application, from one origin. Nothing recorded which. This table does, and it is the
        -- only place a variant's provenance is stored — the rights tables keep keying on the
        -- variant id itself, because a grant belongs to the copy that earned it.
        CREATE TABLE app_variants (
            variant        TEXT PRIMARY KEY,
            logical        TEXT NOT NULL,
            origin         TEXT NOT NULL,
            trust          TEXT NOT NULL,
            provenance     TEXT NOT NULL,
            -- 'derived' (evidence grouped it) or 'user' (a person split or merged it by hand).
            -- The column exists so that a manual override, when it lands, is distinguishable from
            -- a rule's guess and is never silently overwritten by one.
            logical_source TEXT NOT NULL DEFAULT 'derived',
            first_seen_ms  INTEGER NOT NULL
        );
        CREATE INDEX app_variants_logical ON app_variants (logical);

        -- Old id -> current id. A Flatpak export's variant id moved from `desktop-import.<desktop
        -- id>` to `flatpak.<app id>`, and the migration for that is to *resolve* through this
        -- table on read, never to rewrite what is stored elsewhere. A rolled-back binary then
        -- reads exactly the rows it wrote, which is what makes the change reversible at all.
        CREATE TABLE app_variant_aliases (
            alias      TEXT PRIMARY KEY,
            variant    TEXT NOT NULL,
            reason     TEXT NOT NULL,
            created_ms INTEGER NOT NULL
        );
        CREATE INDEX app_variant_aliases_variant ON app_variant_aliases (variant);

        -- The logical application alongside the variant on each association table. Nullable and
        -- unread by an older binary: every INSERT in the tree names its columns, so these are
        -- invisible to one. They are a read model for 'which application did this choice mean',
        -- never the key a default resolves by.
        ALTER TABLE mime_defaults ADD COLUMN logical_app_id TEXT;
        ALTER TABLE extension_defaults ADD COLUMN logical_app_id TEXT;
        ALTER TABLE project_mime_defaults ADD COLUMN logical_app_id TEXT;

        -- Which origin the recorded import came from, so a re-scan can tell a package's entry
        -- from a Flatpak's without re-reading and re-classifying every file.
        ALTER TABLE desktop_import ADD COLUMN origin TEXT NOT NULL DEFAULT 'unknown';
",
    ),
    // `0007`: Homegarden's ledger, one JSON value (`templar_session_core::homegarden::Ledger`)
    // under the key `ledger`, in the same store as the Home Field it tends. Unread by an older
    // binary, which simply never grows a place.
    Migration::additive(
        "0007-home-garden",
        r"
        CREATE TABLE home_garden (
            key        TEXT PRIMARY KEY,
            value_json TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );
",
    ),
    Migration::additive(
        "0008-notification-source-instance",
        r"
        -- A notification can return to the exact managed launch that sent it, including
        -- a terminal completion helper. Older readers ignore this optional association.
        ALTER TABLE notifications_active ADD COLUMN source_instance_id TEXT;
        -- Bind the link to its event: a rolled-back writer replaces event_id without
        -- knowing about either column, and must not carry the old sender forward.
        ALTER TABLE notifications_active ADD COLUMN source_event_id TEXT;
",
    ),
];
