# Files (`templar-filed`)

The file manager is a client, not the owner of file state. `templar-filed` holds the parts that
three different UIs would otherwise each reimplement — and disagree about.

## Browsing is structured service data

`Files.Browse1.Roots` is the shared XDG-places and mounted-drive source list;
`Files.Browse1.ListDirectory` returns one bounded snapshot of real immediate children, including
kind, symlink/hidden state, size and modification time; and `Files.Browse1.Inspect` returns Unix
metadata for an explicitly requested properties pane. For a real directory, Inspect also walks
descendants on a blocking worker and returns regular-file and subfolder counts plus logical and
allocated byte totals. It never follows symlinks; if a descendant cannot be read, the totals are
marked partial rather than silently presented as exact. The service applies folder-first natural
name ordering (`file2` before `file10`) and caps a listing response at 500 entries. Home,
`templar-files` and future chooser panes therefore render the same roots and contents without any
shell view walking the filesystem or inventing a private sort policy. This list ordering is
deliberately different from the Silvaview-derived treemap: treemap area and ordering describe
bytes, while a file list is for finding names.

Home is the first integrated client. Expanding Downloads, Documents, Media or Drives shows a
places-and-mounted-volumes rail and an actionable name/size/modified table. A single click enters
a folder or resolves a file with `Associations1` and launches it with `Launcher1`; the shell
does not fall back to `xdg-open` or execute a path itself. Search hits use those same rows and
actions.

`templar-files` is the standalone client. It keeps only view state: back/forward history, selection,
clipboard intent and rename/location dialogs. Copy, cut/move, paste, rename and Trash are forwarded
to `Files.Operations1`; active journal progress and cancellation remain visible, and Ctrl+Z asks
the service whether the completed operation still has an undo. The persistent right-hand inspector
shows a selected-file preview immediately. Its explicit Properties action combines
`Browse1.Inspect` with `Volumes1.InfoForPath`, exposing type, exact path, size/allocation, folder
totals, times, permissions, owner IDs, link target and containing-drive capacity without making
every selection recursively scan a directory.

## Previews are bounded service results

`Files.Previews1.Render` returns metadata plus a PNG no larger than the requested edge (clamped to
32–768 pixels). It accepts BPG, HEIC/HEIF, PDF, common raster and common video extensions;
PDF rendering is limited to the first page. Still-image and document sources over 512 MiB are
rejected before decode, final PNGs are limited to 16 MiB, external processes have wall-clock and
output limits, and dimensions are checked before bytes cross D-Bus. Videos are seeked and are not
read into service memory, so their container size is not treated as an allocation bound. The GUI
decodes every returned PNG again and verifies that its dimensions match the metadata before
painting it.

The still/PDF routes are first-party Rust code. `bpg-rs` decodes BPG and HEIC/HEIF with dimension,
pixel and memory limits; image-rs decodes ordinary rasters; and the Templar CPU renderer compiles and
paints page zero of a PDF. The Templar renderer declares Rust 1.97, so
`scripts/build-preview-codecs.sh` compiles it into one bounded `templar-preview-codecs` process and
stages it beside `templar-filed`; the main workspace now shares its honest Rust 1.97 floor. JPEG XL previews were removed on
2026-09-29 to drop the `../jpegXL-rs` sibling from the build; `.jxl` files show no preview.
`TEMPLAROS_PREVIEW_CODECS` selects an alternate development binary.

Video thumbnails intentionally use ffmpeg because it has the broadest reliable container and
codec coverage. The service obtains the selected video stream's duration from ffprobe, calculates
`duration × 0.03`, performs a fast seek to that timestamp, and asks ffmpeg for exactly one
aspect-preserving frame. ffmpeg is not a fallback for still images or documents.

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

Before a request enters that journal, the service—not merely the GUI—requires absolute existing
sources, a real destination directory, a single-component rename, unique paths, no filesystem-root
removal, and no copy/move of a directory into itself. A caller cannot bypass those invariants by
speaking D-Bus directly.

`templar-filed` resumes unfinished operations at start. That is the whole reason the journal is in
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

The standalone manager's visible Copy path button requests the `as-shown` form and sends that text
to the compositor clipboard with `wl-copy`; `wl-clipboard` is therefore a runtime dependency, not
an optional development convenience.

## pcmanfm-qt is the file manager

The session's file manager is `pcmanfm-qt` (libfm-qt, Qt 6), not `templar-files`. `templar-files` was
built as a client of the interfaces above, and as a client it works; as a file manager it is too
rudimentary to be the one a person has to use. It stays a workspace member and a buildable
`.deb` — it exercises `Browse1`, `Operations1`, `Previews1` and `Actions1` against a real UI —
but it is no longer in `distro/seeds/selection.toml`, in `templaros-desktop`, or the default for the
`file-manager` role.

`ProviderRole::FileManager` is derived the way `Terminal` is: from `Categories=FileManager` in the
installed entry, at priority 100 when the executable is `pcmanfm-qt` and 50 otherwise
(`templar-session-core/src/providers.rs`). Nothing in the shell names an application: Home's folder
tiles, "Open folder window", the browse pane's door and `project.open-files` all resolve the role.
`inode/directory` is `pcmanfm-qt.desktop`'s own `MimeType=`, so the association broker seeds it
with no curated manifest. pcmanfm-qt must never be started with `--desktop`: Home is the desktop.

Qt 6 selects the Wayland platform plugin on its own when `WAYLAND_DISPLAY` is set and
`qt6-wayland` is installed — verified against the nested compositor, `qt.qpa.wayland` with no
`QT_QPA_PLATFORM` in the environment — and the compositor forces server-side xdg-decoration, so a
pcmanfm-qt window wears TemplarOS chrome like any other.

### What integrates, and what cannot

There is no plugin API in libfm-qt, so integration is exactly what a file manager offers a
distribution from outside:

- **Custom actions** (DES-EMA, `/usr/share/file-manager/actions/*.desktop`, shipped by
  `templaros-cli`): "Add to Home" runs `templar home place %f` over `HomeField1.PlaceFolder`, and
  "Create project here" runs `templar project create-here %f` over `Projects1.Create` with the id
  derived from the folder name. This is the whole of the outbound route.
- **Bookmarks**: filed is the authority. Every `Files.Bookmarks` mutation rewrites
  `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, which is the file libfm-qt's Places sidebar reads. One
  way, deliberately: importing it would make two writers of one list with no answer to which is
  right.
- **Trash**: nothing to do. `templar-filed`'s `Trash1` already implements the freedesktop Trash
  specification on the same directories libfm-qt uses — `$XDG_DATA_HOME/Trash`, the administrator
  `$topdir/.Trash/$uid` when it passes the sticky/non-symlink gate, and `$topdir/.Trash-$uid`
  otherwise — so Home's Trash drawer and pcmanfm-qt's Trash are the same trash.
- **Recents**: not integrated. pcmanfm-qt writes `recently-used.xbel`; `Files.Recents1` keeps its
  own answer in the state store and does not read xbel. Deferred, not planned: the two answer
  different questions ("what did I open" against "where was I last working").
- **Search**: cannot integrate. pcmanfm-qt's Find is its own, and there is no route by which it
  could ask `Files.Search1`. Home's search and FSearch remain the indexed answer.
