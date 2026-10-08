# History and notifications
Every important decision is an `Event` with a cause and actions (undo/restore/open). Notifications are
events (sessiond hosts `org.freedesktop.Notifications`; the shell renders). Coredumps are ingested as
crash events attributed to instances.

## Undo publication

`History1.Undo` commits the domain write, undo marker and replacement history event before
publishing Settings values, provider selections or association defaults. Writers and undo use
Store-before-domain-cache locking; a failed commit leaves the effective cache and durable row
unchanged. Association undo reads its staged database inside the transaction and regenerates
the owned `mimeapps.list` only after COMMIT. Successful undo emits the domain's `Changed` or
`DefaultsChanged` signal as well as `History1.Changed`, so open controls re-read the restored
value. Repeating an undo returns false.

Native private-bus verification on 2026-10-05 injected a deferred foreign-key error at SQLite
COMMIT: provider/settings/association caches, durable rows, exported defaults and undo audit
remained unchanged, with no domain change signal. Removing the fault restored the prior provider
and the setting's schema default. MIME, extension and project-default undo and Forget restored
their domain state, external MIME export and matching public signal payloads. This does not
establish catalogue reload, installed-session or physical-device acceptance.

