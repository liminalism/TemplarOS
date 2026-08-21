# Files (`lege-filed`)

The file manager is a client, not the owner of file state. `lege-filed` holds the parts that
three different UIs would otherwise each reimplement — and disagree about.

## Operations are transactions

Planning is split so that policy is testable without a filesystem: `survey` does the I/O (walks
the sources, totals them, notices existing destinations, asks whether source and destination
share a device) and `plan` is pure. Execution applies **idempotent** steps from a journalled
cursor:

```
copy to a temporary in the destination directory → flush the file and its directory entry →
verify (size, or CRC-32 read back) → expose atomically (rename) → delete the source
```

Because the temporary lives in the destination directory, the final rename never crosses a
filesystem boundary, and because the source is removed last, a crash leaves either the original
or the copy — never neither. A same-device move skips all of it: one atomic `rename(2)`.

Conflicts are decided **once, at plan time** (`fail` / `skip` / `replace` / `keep-both`), so the
journal records what was agreed rather than re-asking after a restart. Undo is planned at commit
time for rename, move and trash. Delete is irreversible and says so before it runs.

`lege-filed` resumes unfinished operations at start. That is the whole reason the journal is in
the service rather than the GUI.

## Storage information is unavoidable

`Files.Volumes1` is the one list the sidebar, the Drives drawer and the file chooser all render,
so none of them can show a volume without also showing how full it is. The **reserve** is the
space kept back for logging in, updating and recovering; an operation that would consume it is
refused, and the override is explicit and recorded. A full root filesystem is almost as
destructive to responsiveness as exhausted RAM.

## Trash

The freedesktop trash, with the per-volume fallback (`$topdir/.Trash-$uid`) so trashing is always
a rename: it must never turn into a long copy, and must never fail because a disk is full. The
`.trashinfo` record is written before the rename, so a crash between the two leaves a stale
record rather than an unexplainable file.

## One answer about "the last folder"

`Files.Recents1` holds recent files and the last-used directory, in the one per-user state store.
The order is: last directory for *this application* → last for this operation type → the open
project's root → fallback, and every suggestion carries the reason it was made. A directory that
has since been deleted is never suggested.

Two forks that each remembered their own last folder is the problem this replaces.

## Search

`Files.Search1` indexes names, paths and metadata over registered roots — which is what makes
every drawer searchable and lets a drawer *be* a saved query. Deliberately **not** content
indexing: a daemon that reads every file the user owns costs I/O, battery and a far larger
privacy surface than the drawers need. An empty query matches nothing.

## Path actions

`Files.Actions1` owns "copy canonical path", "copy URI", "copy shell-escaped path" and "open a
terminal here" — otherwise a service menu, a downloaded script or a local patch in every file
manager, each quoting differently. They are small enough to be exact, so every client gets the
same answer.
