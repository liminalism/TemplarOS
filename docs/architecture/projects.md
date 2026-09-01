# Projects
A project is a place to work, not a numbered desktop: a root and further directories, the
applications to restore, environment, terminal directories, mounts, and loose references to things
that are not its own files. One JSON blob per project in sessiond's store, edited as a unit and read
additively in both directions (`interfaces/schemas/project.schema.json`).

`Projects1` exposes the whole record through `Describe` and `ListDetailed`, and edits it through
`Update`, which takes a partial object over an allowlist of the user-editable keys — `id`, `apps`
and the timestamps belong to the daemon, and a client that names one is refused rather than quietly
ignored. `SetState` archives a project without hiding it: archived stays searchable and openable.
`GetCurrent`/`SetCurrent` name the project the session is working in; it is persisted in
`settings_values` under `_internal.projects.current`, and the current project is what
`Files.Recents1.SetProject` is told so recent-file suggestions follow the work.

`importance` has two values, Normal and Important, because that is the distinction a person can
answer honestly. It replaces the never-implemented `priority_boost`. An Important project that is
open asks the resource governor to protect the instances it launched, through the existing
`Resources1.SetInstanceHints`; closing it, or demoting it, pushes the manifest's own values back.
No interface changed to make that work, and no graded channel exists until the governor has a weight
to multiply one by.

A remembered application may name a logical application rather than one variant, so a project opened
after the Flatpak replaced the distribution package still opens the same application; the variant
that actually launched is recorded, and a project that can resolve neither says so and skips it.
