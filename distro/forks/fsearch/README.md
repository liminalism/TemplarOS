# FSearch using Index1

Upstream: <https://github.com/cboxdoerfer/fsearch>, exact commit
`05ed650ab101ee3d61795fa5eeeb5c32e1e366c3` (2026-08-27).
The application is GPL-2.0-or-later. The upstream checkout stays in the ignored
`forks/fsearch` directory. All downstream changes are in
`debian/patches/shared-templaros-index.patch`; no published TemplarOS Git branch is claimed.
The manifest pins upstream, and the packaging preflight requires that exact, clean checkout.

## Status — owner amendment 2026-10-08

This fork is a qualified prototype. The owner deferred WP14's shell/FSearch
generation comparison and selected a future integrated Index1 search GUI with a
large FSearch-style window. WP16's fork shipping/default adoption is deferred.
The retained package/private GTK evidence shows this prototype uses Index1; it
does not establish the final provider or integrate PCManFM Find.

## Build

On native Linux with the real build dependencies in `debian/control` installed:

```sh
scripts/build-fork-deb.sh fsearch --source /path/to/clean/pinned/fsearch
```

The builder copies the source, applies the quilt patch and builds
`templaros-fsearch` into `target/debian`. Use a clean checkout: a development
checkout with this patch already applied would apply it twice. The package enables
Meson's `templaros_index`; the upstream option remains off by default.
The executable remains `fsearch`; its desktop and application ID is
`org.templaros.FSearch`. It depends on `templar-filed` and `templar-settings`, and
replaces the distribution's `fsearch` package.

Index mode excludes the scanner, on-disk database, index store, rescan scheduler
and inotify/fanotify monitor source files from the object graph. All Index1 RPCs
run on the database worker. There are 32 pages of 100 rows per view; additional
AND terms use an integer projection, and selected native rows are retained only
within the service's retained result bound.

This continuation built the DEB using genuine Ubuntu 26.04 development packages
configured in an owned native PRoot directory, with no host package installation.
PRoot 5.5 required a local `openat2` path-translation patch for its zero-resolution
mask calls, plus `TAR_OPTIONS=--absolute-names` for this owned source staging.
Nonzero `openat2` resolution masks remain rejected by that helper. These are
isolated build-environment accommodations, not package or application settings.
The helper patch, package logs and binary hashes are retained in
`.agent/scratch/index-wp14`.

## Search and actions

Supported: quoted words, implicit AND, terms containing `/`, `file:`, `folder:`,
`size:` comparisons and `dm:`/`datemodified:` ISO8601 comparisons. Sort by name,
path, size or modified time. Bare size suffixes and `KiB` use powers of 1024;
`KB` uses powers of 1000. Several bounds intersect.

For AND, the longest term is the service needle. Other terms filter **only the
retained rows**. The notice states both the retained bound and the filtered count;
it does not claim a complete intersection beyond that bound.

Regex, wildcard/grouping, OR/NOT/exclusion, unknown fields such as `content:`, exact
and case-sensitive modes, and the Search in Path toggle report unsupported.
Use a term containing `/` for path search. Type and extension sort controls are
disabled. Index mode installs a help page describing these actual capabilities.

The notice uses the Search response's generation and root freshness. Busy retries
once after 50 ms. SnapshotRequired refreshes the query; Unavailable displays its
reason. Cancelled requests are discarded. A failed selection page clears partial
selection before exposing file actions. Select All includes every retained row.
Selection survives sorting and renaming by entry ID. Display names use
`g_filename_display_name`; actions use validated, canonical base64 native bytes.
Icon decoration guesses from indexed name/kind without probing offline paths.

Indexed roots are read-only here. Preferences open the existing Settings route,
`templar-settings --page index`. The plan's `templar settings files` command does
not exist. FSearch neither saves includes/excludes nor creates `fsearch.db`.
Single-instance `--search` changes the existing window and returns success.

## Owned integration

```sh
bash distro/forks/fsearch/tests/remote-index.sh \
  --filed /path/to/templar-filed --fsearch /path/to/extracted/usr/bin/fsearch \
  --probe /path/to/build/src/tests/remote_index_probe --output /path/to/evidence
bash distro/forks/fsearch/tests/fault-index.sh \
  --probe /path/to/build/src/tests/remote_index_probe --output /path/to/fault-evidence
```

The first script owns its bus, Xvfb, HOME/XDG directories and 5,001-entry fixture,
including an invalid UTF-8 filename. Pass `--xvfb` and `--strace` explicitly when
those tools are extracted rather than installed. It checks native paging, complete
selection, sorting, renamed selection, refresh, AND, unsupported queries and
single-instance activation, and captures the private display. The second uses a
controlled protocol stub for Busy retry/exhaustion, Search/page SnapshotRequired,
failed selection, Unavailable/recovery and cancellation.

The private bus has no activation directories. Fixture GTK uses
`gtk-im-context-simple`, local GIO and no portal; a real desktop's GTK input method
may watch its own IBus configuration independently of indexing. The application
does not force a different input method on the person.

The traced directory-read allowlist observed on Ubuntu 26.04 is:

- Font configuration: `/etc/fonts`, `/usr/share/fontconfig`.
- GTK/icons: `/usr/share/themes`, `/usr/share/icons`, `/usr/share/pixmaps`,
  `/var/lib/flatpak/exports/share/icons`, `/var/lib/snapd/desktop/icons`.
- Runtime modules: `/usr/lib/x86_64-linux-gnu/gio/modules`,
  `/usr/share/drirc.d`, `/usr/share/glycin-loaders`.
- Owned Glycin/bubblewrap loader setup: one level of `/`, the traced loader's
  `/proc/<pid>/fd`, and NSS's `/run/systemd/userdb`.

The trace includes executable identities. Any other directory read, or any read
inside the fixture tree, fails. No recursive fixture enumeration, watch FD or
FSearch database is allowed. This broader runtime allowlist is recorded explicitly;
it is not a claim that GTK reads theme directories exclusively.

WP16 fork adoption is owner-deferred pending the future integrated Index1 GUI.
Installed laptop, volumes and physical HDMI qualification remain owner-run WP17 gates.
