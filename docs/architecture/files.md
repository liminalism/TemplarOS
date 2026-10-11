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

`Actions1.RenderNative(path_b64, form)` is the additive byte-exact clipboard boundary. URI
percent-encoding preserves arbitrary Unix filename octets; shell-quoted uses existing POSIX
single quotes for UTF-8 and ANSI-C octal quoting for non-UTF8 (Bash/Ksh/Zsh/POSIX.1-2024).
Canonical, as-shown, parent-directory and name plain-text results explicitly refuse any
unrepresentable UTF-8 rather than naming another file. The existing `Render` signature stays.

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

## Shared index freshness and native policy

`Files.Index1` is the typed index boundary. Authoritative policies are keyed by native path bytes
in session state, independently of the disposable SQLite index cache. Configure/remove uses the
observed policy hash; reconciliation also carries the observed cache identity. Cache deletion
does not discard policy or recreate a deliberately removed Home root.

Visible consumers hold an observation token and close replaced search cursors explicitly.
Paging refuses metadata written after the pinned generation with `SnapshotRequired`.
Namespace completeness and measured metadata are separate: a policy-change pass can return a
current namespace with unverified sizes. Size/time/mode/aggregate bulk requests queue metadata
repair through the daemon's watcher control without blocking or relabelling the captured frame.
The next frame may be current and measured; an unverified frame remains visibly partial.

Bulk snapshots use fully sealed memfds published read-only. Native private-bus execution covered
policy CAS, raw-byte root/file paths, cursor mutation rejection, metadata repair, snapshot decoding,
URI/Bash round-trips, and cache-deletion identity fencing.

Index calls run on bounded Interactive, Bulk and Maintenance worker lanes. Search and paging
use Interactive; snapshots use Bulk; large change reads and maintenance have separate admission.
Caller limits and latest client tags cancel superseded searches; the hard-capped reader pool and
SQLite interruption bound outstanding work. Snapshot writers stream through a 256 KiB buffer,
enforce byte/entry/time limits and patch the header before sealing. Readers validate all four
seals, exact length and manifest identity before borrowing entries from an immutable mmap.

Acquisition gives each path to the most specific registered root. Search acceleration uses
root-local immutable shards and bounded generation-fenced overlays; declared path/other SQL
fallbacks remain. Current fixture p95 gains do not qualify the remaining SQL p99 tails.
`Roots` diagnostics expose availability, main/WAL/SHM sizes, queues, readers, cursors, shards,
overlays and server-held snapshot descriptors. Commit-triggered checkpoints do not add an idle
polling timer. Mountinfo presence and measured UUID/fsid confidence fence offline roots;
different-volume acceptance is an explicit fingerprint-fenced Settings action. A unique
strong UUID/mount-root match at another mount supplies a native-byte location hint for
offline roots. Settings offers an explicit new registration with the same policy, followed
by fenced removal of the old registration and a full scan. Paths are never rewritten by
discovery. [Injected-provider relocation tests](../measurements/index-volumes-2026-10-08-relocation.md)
pass; the real mount journey and actual NTFS unplug/return remain unqualified.

The first-party Apps/Files launcher consumes this index directly. The retired FSearch prototype previously consumed it without a local scanner, watcher or `fsearch.db`.
Silvaview's optional Linux service mode consumes summary-first snapshots, labels indexed logical
size and freshness, and expands native subtrees on demand. Missing normal directory aggregates
remain partial. Service failure or stale frames never silently start a direct scan. The actual
private FSearch GTK and software nested Silvaview observations are retained in
[WP14](../measurements/index-fsearch-2026-10-08-wp14.md) and
[WP15](../measurements/index-silvaview-2026-10-08-wp15.md); installed sharing remains open.

Warm daemon startup publishes the previous committed namespace before its asynchronous downtime
repair finishes; restarting a consumer never synchronously walks a root. Overflow and downtime
repair must enumerate membership rather than trust a directory's cached mtime/ctime: watcher
events can refresh that metadata even when dropped child events left the namespace incomplete.
An incomplete prior namespace also cannot be made complete by timestamp equality or a disjoint
subtree repair. Known-current namespace-only repairs retain their separate unverified metadata
axis; pinned cursors still require `SnapshotRequired` after affected rows change.

Native Ubuntu 26.04 private-daemon proof on 2026-10-06 observed real kernel overflow, all 12,000
created names restored, stale/building/current signals, useful previous-generation searches
during repair, partial-budget retention, and caller-loss dormancy/reactivation. A separate
50,001-entry retained-observation sample measured zero watcher schedules/context switches and
zero process read/write I/O over 60 seconds, with unchanged root generation and scan timestamps.
The benchmark now propagates apply/source/deadline/coverage failures and runs required
watch-before-read reconciliation before reporting settled, rather than interpreting empty
mutation batches as proof. It owns and reaps its private bus and daemon and isolates both bus
addresses plus HOME/XDG paths. See `docs/measurements/index-baseline-2026-09-04.md` for scope.
Physical Btrfs downtime delta across reboot and installed desktop acceptance remain unproven;
these private fixtures do not adopt the proposed checkpoint protocol.

## Path actions

`Files.Actions1` owns "copy canonical path", "copy URI", "copy shell-escaped path" and "open a
terminal here" — otherwise a service menu, a downloaded script or a local patch in every file
manager, each quoting differently. They are small enough to be exact, so every client gets the
same answer.

The standalone manager's visible Copy path button requests the `as-shown` form and hands the
result to its retained PixelKit native clipboard worker, shared with text fields. Completion
reports success only after clipboard ownership succeeds; no `wl-copy` subprocess or separate
Wayland-only clipboard path is used. Paths that cannot be represented as UTF-8 report an error
rather than copying a lossy filename.

An owned native X11 session on 2026-10-06 copied a pathname containing spaces, café, CJK,
quotes and `$(touch EXECUTED);` from the real Files window into Chrome via native Ctrl+V.
The pasted bytes matched the pathname and the command-shaped filename produced no sentinel.
This proves that private-session path action, not installed-session or physical Wayland acceptance.

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
`inode/directory` is `pcmanfm-qt.desktop`'s own `MimeType=`, so it is a handler without a
curated manifest. The vendor `distro/policy/mimeapps/mimeapps.list` separately selects that
desktop ID as the default: advertising a MIME type alone does not choose it over other handlers.
On first startup sessiond imports defaults from the XDG configuration and data directories,
including desktop-specific files, in preference order; user selections win while absent keys
inherit vendor policy. Later startup uses the stored choices and exports their effective view.
pcmanfm-qt must never be started with `--desktop`: Home is the desktop.

Qt 6 selects the Wayland platform plugin on its own when `WAYLAND_DISPLAY` is set and
`qt6-wayland` is installed — verified against the nested compositor, `qt.qpa.wayland` with no
`QT_QPA_PLATFORM` in the environment — and the compositor forces server-side xdg-decoration, so a
pcmanfm-qt window wears TemplarOS chrome like any other.

### Terminal and icons

PCManFM-Qt's built-in defaults are `[System] Terminal=xterm` and `FallbackIconThemeName=oxygen`;
a TemplarOS machine has neither. Nothing in the session gives Qt an icon theme, so PCManFM-Qt
uses the fallback it names, and with that theme missing every file and folder in the icon view
is drawn as text alone.

- **Terminal**: `templar-terminal` (shipped by `templaros-cli`; it runs `templar terminal`).
  It resolves the Terminal provider role through `Providers1.Report` and starts the provider with
  `Launcher1.Launch`, so the terminal gets its own `app-templaros-<id>@<instance>.service` unit
  and identity instead of landing in PCManFM-Qt's cgroup. `Launcher1.Launch` takes only an
  argument vector and every launched unit starts in the home directory, so the folder travels as
  `--directory DIR` (kitty's spelling, the same form the shell's "Open terminal here" passes the
  role), followed by `--hold` and `-e CMD ARGS…` when libfm-qt asks to run a command; a provider
  that spells these differently needs its own form, and none is declared today. libfm-qt starts
  the terminal command *in* the folder ("Open in Terminal"), so with no `--directory` the
  current directory is used. Only when `org.templaros.Session` is unreachable (no session bus,
  or nobody owns the name) is there no role to resolve; then `x-terminal-emulator` is executed
  directly in the folder, with `-e` and without `--hold`. `templar terminal --dry-run` prints the
  route, application and arguments without launching.
- **Running a command in the terminal** ("Execute in Terminal", `Terminal=true` entries) goes
  through libfm-qt's `expand_terminal`, which only builds `<terminal> -e CMD` for a terminal it
  has a `terminals.list` group for and otherwise runs `xterm -e CMD` whatever the setting says.
  `/usr/share/libfm-qt/terminals.list` belongs to `libfm-qt5-common`, so the group
  (`open_arg=-e`, `noclose_arg=--hold -e`) is added to the per-user
  `$XDG_DATA_HOME/libfm-qt/terminals.list`; an existing `[templar-terminal]` group is left as the
  user wrote it.
- **Icons**: `FallbackIconThemeName` is the first installed of Papirus, Yaru, Adwaita and hicolor.
  Papirus is what `templaros-desktop` installs and covers every freedesktop name PCManFM-Qt asks
  for, toolbar actions included; Adwaita on Ubuntu 26.04 ships few full-colour action icons.

A fresh account gets `/etc/xdg/pcmanfm-qt/default/settings.conf` (`Terminal=templar-terminal`,
`FallbackIconThemeName=Papirus`), which PCManFM-Qt reads only while
`$XDG_CONFIG_HOME/pcmanfm-qt/default/` does not exist. An existing profile is repaired by
`templar-sessiond` at startup, before it claims its bus name and so before anything can launch
PCManFM-Qt (`templar-session-core/src/pcmanfm.rs`): a `Terminal` whose program is not on `PATH`, or
a `FallbackIconThemeName` with no `<theme>/index.theme` under `$XDG_DATA_HOME/icons`, `~/.icons`
or `$XDG_DATA_DIRS/icons`, is rewritten to the default; an absent key counts as PCManFM-Qt's
built-in default. Valid choices (`foot`, `breeze`, …) and every other byte of the file are kept.
The write is atomic, follows a symlinked file, and is abandoned if the file changes meanwhile
(PCManFM-Qt saves its settings on exit, so a running copy can write the old values back; the next
session start repairs them again). `templar pcmanfm repair [--backup]` runs the same repair now,
`--backup` first copying the original to `settings.conf.templaros-bak` (an existing backup is
never overwritten).

### What integrates, and what cannot

There is no plugin API in libfm-qt, so integration is exactly what a file manager offers a
distribution from outside:

- **Custom actions** (DES-EMA, `/usr/share/file-manager/actions/*.desktop`, shipped by
  `templaros-cli`): "Add to Home" runs `templar home place %f` over `HomeField1.PlaceFolder`, and
  "Create project here" runs `templar project create-here %f` over `Projects1.Create` with the id
  derived from the folder name. Archive actions run locally, as described below.
- **Bookmarks**: filed owns Home places only, not PCManFM-Qt's list. `Browse1.SetBookmark` never
  exports implicitly. `Browse1.ExportBookmarks` / `templar files bookmarks export` explicitly
  adds missing places to `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, preserving native labels and
  unmanaged bytes. A unique durable backup and `bookmarks.templaros-owned.json` record which
  lines Templar actually added; existing matching URIs are not adopted. Removing a Home bookmark
  never deletes a GTK entry. Publication is atomic; intervening native writes are detected,
  restored when possible and retained at the returned conflict recovery path. No import or
  destructive synchronization is performed.
- **Trash**: nothing to do. `templar-filed`'s `Trash1` already implements the freedesktop Trash
  specification on the same directories libfm-qt uses — `$XDG_DATA_HOME/Trash`, the administrator
  `$topdir/.Trash/$uid` when it passes the sticky/non-symlink gate, and `$topdir/.Trash-$uid`
  otherwise — so Home's Trash drawer and pcmanfm-qt's Trash are the same trash.
- **External operations**: PCManFM-Qt create/update/rename/move/trash changes enter the shared
  index through the native watcher. They do not become fabricated filed journal/undo records.
- **Recents**: not integrated. pcmanfm-qt writes `recently-used.xbel`; `Files.Recents1` keeps its
  own answer in the state store and does not read xbel. Deferred, not planned: the two answer
  different questions ("what did I open" against "where was I last working").
- **Search** (owner direction, 2026-10-09): one PixelKit launcher switches between Apps and Files
  while retaining typed text. The ordinary Files mode searches everywhere through `Files.Index1`;
  the shipped PCManFM **Search here…** custom action invokes `templar files search-here <directory>`
  to open that same popup with explicit directory scope. PCManFM's own Find remains available.
  FSearch provider routing and package adoption are retired; WP14 measurements remain historical
  prototype evidence, not a current shipping dependency. Native path bytes remain target identity;
  search hiding/aliases/favorites are reversible Settings1 metadata, not filesystem operations.


Native Ubuntu 26.04 observation on 2026-10-06 used actual PCManFM-Qt on an owned X11 display
and private bus. Its `FileManager1` owner PID matched the launched process; `ShowItems`
visibly selected the fixture `alpha.txt`, and its Places rail showed the original native
labels alongside the additively exported folder. `ShowFolders` accepted that folder.
Real `Browse1` calls preserved the backup bytes, retained GTK entries after unbookmarking,
and preserved an intervening external label without duplicating its URI. These observations
are not installed-session relogin or external-operation undo claims.

The same actual PCManFM producer subsequently renamed a literal UTF-8/metacharacter file,
Cut/Pasted it into a folder, and trashed it through native GIO VFS. `Index1.ChangesSince`
reported entry **2** moving twice (generations 3 and 4), then removal at generation 5;
search no longer returned it. `Trash1.List` retained the exact original and trashed paths
and the exact 42-byte payload. The primary store still had zero `file_operations` and
zero `file_undo` rows: external native operations do not invent Templar undo.
This fixture required actual GVFS readiness before starting fresh PCManFM. An earlier
local-VFS-only Delete prompt was canceled, not approved as a permanent deletion or
reported as Trash success. The observed change stream and payload hash are retained in
`.agent/scratch/native-external-mutations-20261006.json`.

A further independently owned native X11/private-bus fixture on 2026-10-06 copied the source
vendor policy into isolated `XDG_CONFIG_DIRS` before starting the native sessiond. The real
`/usr/share/applications` catalogue selected PCManFM-Qt at priority 100 over Files at 50,
and `ResolveFile` returned `global-mime-default` for the literal directory
`literal café 家庭 $(touch EXECUTED); 'literal'`. This exposed and corrected the importer
ignoring `XDG_CONFIG_DIRS` and stopping at the first preference file instead of inheriting
missing defaults. The corrected fresh database resolved the vendor desktop ID.

Native `gio open`, explicit desktop-entry launch, and a real PCManFM **Open With →
PCManFM-Qt File Manager** click opened that directory. `FileManager1.ShowItems` selected its
literal UTF-8/metacharacter filename; Properties showed its exact name, containing path and
43-byte size, and byte/hash comparisons preserved the contents. Process arguments contained
the canonical directory and never `--desktop`; the native breadcrumb address showed the
literal folder name. No `EXECUTED` sentinel
appeared. These are actual native application routes, not a verified broker-launch claim.
The isolated bus had no genuine `org.freedesktop.systemd1` owner and the process had no writable
delegated cgroup. `Launcher1.Launch` returned an error; no fake manager, live user manager,
or trust upgrade for a user-owned desktop entry was used. Installed-session broker launch,
relogin and physical Wayland acceptance remain separate prerequisites.

The same fixture invoked—not merely displayed—the freshly copied source DES-EMA **Add to
Home** and **Create project here** actions with the native source-built `templar` first on PATH.
The Home service document gained the exact folder tile and revision, and Projects listed the
exact root/name with its safely derived project ID. With no Home shell running, the fixture
first initialized the real primary field through `HomeField1.Get`; it does not claim a visible
Home tile render. Retained screenshots, service replies, literal payload/archive bytes and
owned-process teardown are in the AKR-kept `.agent/scratch/native-file-routes-89_fntxv/`;
the catalogue-only policy result is `.agent/scratch/native-file-catalogue-20261006.json`.


### Archive context menu

`templaros-cli` ships a directly visible **Compress to ZIP** action and an **Extract**
submenu with **Extract here**, **Extract to smart new folder here**, and **Extract to
other directory…**. DES-EMA `%F` passes the entire selection to one process: compression
creates one ZIP; extraction processes each selected archive, including mixed formats.
The actions are available for local files; compression also accepts directories.
Extraction stays visible for unrecognized file types so mixed selections do not disappear
because of libfm-qt's all-selected-items MIME matching. Non-archives produce an error.

The commands are:

```sh
templar archive compress [--output PATH] [--gui] -- FILE_OR_FOLDER...
templar archive extract --mode here [--gui] -- ARCHIVE...
templar archive extract --mode smart [--gui] -- ARCHIVE...
templar archive extract --mode directory [--directory PATH] [--gui] -- ARCHIVE...
```

Without `--output`, one source becomes `source-name.zip`, while a multiple selection
becomes `Archive.zip` beside the first source. ZIP and smart-folder collisions are
numbered instead of overwritten, including an explicit compression output path.
Smart extraction names the folder after the archive, stripping compound suffixes
(`package.tar.gz` becomes `package`), and flattens a sole top-level directory to avoid
an unnecessary wrapper. Here and other-directory extraction refuse existing top-level
names rather than merge or overwrite. Earlier successful outputs remain available if
a later archive fails, and the error dialog lists them.

The other-directory action opens Zenity's native folder chooser, initially beside the
first archive. Zenity also provides cancellable progress, error dialogs and encrypted
ZIP password entry; it is a runtime dependency, not an archive manager. Canceling the
chooser writes nothing. Canceling an operation removes its uncommitted staging files.
Successful output paths are printed on stdout. No desktop-service connection is required.

`crates/templar-archive` creates DEFLATE ZIPs with the Rust `zip` crate and a `zlib-rs`
backend. Extraction streams through the system `libarchive` with reused 128 KiB buffers:
ZIP, 7z, RAR/RAR5, TAR, CAB, ISO, ar/DEB, CPIO, LHA/LZH, XAR and WARC, with the installed
codec support for gzip, bzip2, xz/LZMA, zstd, LZ4 and other libarchive filters. Standalone
compressed streams are supported too; DEB/ar members are not recursively unpacked.
Gzip checksum validation uses a single-pass Rust decoder because libarchive's gzip
filter does not validate its trailer. Optional codecs may require host codec libraries
or helpers. Encrypted RAR/7z and unsupported compression/encryption methods fail
explicitly; encrypted ZIP uses the backend's supported methods. ZIP symlink integrity
verification supports stored/DEFLATE entries, not gzip-wrapped ZIP symlinks.

Extraction validates private same-filesystem staging before publishing with atomic
no-clobber renames. Absolute/parent/drive paths, escaping or cyclic links, conflicting
members and special files are rejected. Safe symbolic and hard links and ordinary
permission bits are preserved; ownership, timestamps, ACLs, xattrs and special permission
bits are not restored. ZIP creation does not follow source symlinks and rejects
non-UTF-8 names, literal backslashes and links escaping the selected trees. The native
build needs `libarchive-dev` and pkg-config; runtime needs Linux `/proc/self/fd`,
`C.UTF-8`, libarchive 3.4+ and a filesystem supporting `renameat2(RENAME_NOREPLACE)`.
The archive command and menu do not change how PCManFM-Qt receives TemplarOS window chrome.

In that owned native fixture, actual PCManFM source DES-EMA **Compress to ZIP** and **Extract
to smart new folder here** clicks launched the native source helper with the literal selected
path. ZIP member names and extracted bytes matched the original 43-byte payload; smart
extraction chose a noncolliding numbered directory instead of replacing the original file.
Screenshots show the native choices, published outputs and exact selection properties.
The final runs used owned logfile stdout: an initial harness incorrectly closed the inherited
GIO stdout pipe, causing a Broken pipe diagnostic after publication, so that contaminated
run is recorded but is not the clean action proof. Archive behavior was not changed.
