# Associations (`templar-associationd`)
Order: project MIME default → explicit extension override → global MIME default → generic-type extension
fallback → ask. Detected type beats extension. Choice UI distinguishes open once / always for MIME / always
for extension / always in this project / ask when extension and content disagree. `mimeapps.list` is
*generated* by the broker (import once, then own it). Unavailable handlers are omitted from the
effective compatibility view; explicit stored selections remain intact for reinstall/recovery.

## Committed defaults and interoperability

Association writers and `History1.Undo` acquire Store before the engine and publish cached
defaults only after COMMIT. Undo loads its staged database inside the same transaction, rather
than racing a later writer while reloading. A rejected COMMIT leaves cache, audit and exported
defaults unchanged and emits no `DefaultsChanged`.

The broker atomically regenerates its owned `mimeapps.list` at startup, after mutations,
association undo and catalogue changes. Catalogue publication refreshes compatibility defaults
before emitting `ApplicationsChanged`. Export uses the same installed-copy alias/sibling
resolution and MIME-capability checks as the owned association resolver, rather than exporting
stale stored IDs. An export failure is logged separately from a committed preference: read-only
recovery storage does not pretend the durable write failed. Successful undo emits the same
scope/key signal shape as the corresponding mutation, including public project keys rather
than the internal audit delimiter.

Extension keys are normalized before both persistence and cached publication. Native private-bus
execution covered MIME, dotted extension and project defaults, Forget and each undo; actual
external MIME defaults followed startup/mutation/undo. Deferred COMMIT rejection preserved the
chosen default, export and signals. A further actual private-bus/registry/filesystem regression
observed startup export, a renamed-copy alias reflected before `ApplicationsChanged`, matching
`ResolveFile` substitution, and unavailable export removal on uninstall while retaining the
explicit SQLite selection. A real SQL-aborted grouping transaction preserved the previously
committed logical index; grouping now publishes only after variant-row commit. Installed-session
acceptance remains separate.
