# History and notifications
Every important decision is an `Event` with a cause and actions (undo/restore/open). Notifications are
events (sessiond hosts `org.freedesktop.Notifications`; the shell renders). Coredumps are ingested as
crash events attributed to instances.
