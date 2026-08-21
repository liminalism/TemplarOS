# lege-files — the COSMIC Files fork

Upstream: `pop-os/cosmic-files`, pinned in `distro/forks/manifest.toml` (ADR-0005). Rewriting a
file manager would spend months on edge cases without touching the design; the point of the fork
is to **remove state from the GUI**, not to add features to it.

Patches, and only these:

### 1. Operations go through the service, not the process

Every copy, move, rename, trash and delete becomes
`org.legeos.Files.Operations1.Start(kind_json, options_json, app_id)`, with the progress bar
driven by the `Progress` signal and the cancel button by `Cancel`. Undo is `CanUndo` then `Undo`
— and where `CanUndo` says no, the reason it returns is what the UI shows.

The consequence worth the patch: closing the file manager mid-copy no longer stops the copy, and
a crash mid-copy resumes rather than leaving an unexplainable half-written directory. Delete is
the one operation with no undo, and the confirmation says so before it runs, not after.

### 2. Storage information is not behind a menu

The sidebar shows remaining space beside every drive, and the current directory's header shows
containing volume, free/used/total, the reserve, read-only or degraded state, filesystem type and
the pending operation size — all from `Files.Volumes1.List` and `InfoForPath`. Before a paste,
`CheckReserve` decides whether it proceeds, warns, or requires an explicit override.

Nobody should have to open a disk utility to discover that a drive is nearly full.

### 3. One answer about "the last folder"

Drop the fork's own last-directory memory. Ask `Files.Recents1.SuggestDirectory(app_id,
operation)` and report where the user actually ended up with `RecordDirectory`. The portal and
the Home Surface ask the same question, so the three cannot drift apart — which is the whole
reason the separate patches in the old setup existed.

The Recent drawer is `Recents1.List`; opening a file records it with `Record`.

### 4. Path actions come from the service

"Copy canonical path", "copy URI", "copy shell-escaped path" and "open a terminal here" are
`Files.Actions1.Render` and `TerminalDirectory`, not local scripts or a downloaded service menu.
The quoting is then identical everywhere, including for names containing quotes and newlines.

### 5. Opening a file is not the file manager's decision

Double-click and "Open with" call `org.legeos.Associations1.ResolveFile`, and launch through
`org.legeos.Launcher1.Launch` so the application gets a canonical identity and its own cgroup.
`Associations1.Explain` backs a "why this application?" affordance — the detected type beats the
extension, and the user can see that it did.

### 6. Search is the service's index

The search field queries `Files.Search1.Search`; a saved search is `SaveQuery`, which is also
what lets a Home Surface drawer *be* a query rather than a directory.

---

Manifest: `distro/app-manifests/org.legeos.Files.json`. The fork declares the `file-manager`
provider role, so exactly one file manager autostarts however many are installed.
