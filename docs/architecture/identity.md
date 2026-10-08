# Application identity (`templar-appd`)
One canonical `AppId`; aliases are evidence. Verified identity requires a registered launch whose
actual user-manager `ControlGroup` matches the PID. A self-describing unit name is not authority.
Confidence: unknown / claim-only / verified-launch / verified-launch-claim-mismatch. A unique
claim match remains advisory, never a verified launch. Manifests: `/usr/share/templaros/apps`,
`/etc/templaros/apps`, `$XDG_DATA_HOME/templaros/apps`. Desktop entries without a manifest are imported
(`templar-appd::desktop_entry`), keyed per origin namespace so a Flatpak export and a distribution
package sharing a desktop id stay two applications instead of one silently replacing the other.

Window identity resolves off the compositor thread. Each answer is delivered through a calloop
channel to `WindowService::update_identity`, then published to the shell/governor snapshot.
Destroyed windows and unchanged identities are ignored. The resolver checks PID/start-time
continuity before delivery; there is no frame-polled answer queue.

Desktop discovery watches native file identity, size, and nanosecond mtime/ctime every 15 seconds.
A rescan prepares all installed copies, commits alias/bookkeeping/trust-withdrawal/removal changes,
then publishes the manifest batch under one registry write lock. A later-copy SQLite failure
leaves every prior executable and MIME declaration intact; unchanged filesystem input is retried.
Edited imported copies are not mistaken for native manifests. Uninstall removes effective provider
declarations and emits catalogue/provider changes, but retains explicit selections as stale choices.
Running instances and workloads are not erased when their installed manifest disappears.

Native private-bus and filesystem/SQLite runs observed edited executable/MIME metadata, failed
second-copy publication with both prior copies retained after the fix, unchanged-source retry,
and uninstall/provider retirement without erasing explicit selection. Installed-session acceptance
is separate; no installed service was restarted.

## External launches and permission boundaries

An external desktop launch is not verified merely because its executable or declared app ID
matches an installed manifest. Actual `GDesktopAppInfo` (`gio launch`) native execution used
the imported desktop entry and kernel PID/cgroup, the private bus's unique-sender PID,
compositor integration's real `/proc` start-time helper and a live kernel pidfd. Empty claims
resolved to `Unknown`; a forged matching app ID resolved only to `ClaimOnly` with no instance.
The actual `PeerCgroupVariant`/`Providers1.Declare` path refused provider/capability declaration
and recorded the refusal; protected lifecycle report methods also returned `AccessDenied`.
Neither desktop metadata nor titles/process names establish an application grant.

The same real process was admitted by pidfd into an exact owned Flatpak-shaped transient
scope. Native resource discovery reported `ResourceIdentity::Flatpak` advisory metadata,
while `AppIdentity1` and the provider gate still refused to promote it to a verified launch.
The scope name is not evidence of actual Flatpak sandbox confinement. Unknown-parent explicit
workloads likewise remain separate `WorkloadId` resource identities, not guessed `AppId`
grants. GUI PCManFM/portal launch journeys and installed sandbox enforcement remain separate
acceptance observations; no live service-name owner or installed session was changed.


## Variants and logical applications
An `AppId` names a **variant**: one installed copy, from one origin. A person sees one Firefox; the
system may hold the Debian package, the Flathub build and a `.desktop` file they wrote themselves.
`AppProvenance` (`crates/templar-types/src/variant.rs`, `interfaces/schemas/app-variant.schema.json`)
records where a copy came from — origin, trust tier, source path, package ref, version, launch
mechanism, update owner — and `crates/templar-appd/src/variants.rs` groups copies under a
`LogicalAppId` on evidence: a declared logical app or alias, a shared desktop-entry id across
origins, a flatpak id named in another's aliases, a shared `StartupWMClass` *and* name, or a shared
resolved executable. Minted logical ids are persisted, so they survive an uninstall.

The last two rules are resemblance rather than identity, and a *suite* resembles itself completely:
a settings application ships one entry per destination, all with one window class and one binary.
An installed entry whose desktop id extends another installed entry's by a dotted segment is a panel
of it, and the resemblance rules do not apply to it — thirty-two panels stay thirty-two
destinations. The identity rules still do, so a Flatpak and a package of the same panel merge.

The **default variant** is what launches when a caller names only the application, and it is the
entry a launcher draws the row from. Displayability comes first in that election: an entry its own
author marked `NoDisplay` — a URL handler, a MIME shim — must never be the face of an application
that has a displayable sibling. Then trust, then origin, then the copy that has been installed
longest, then the id.

Grouping is presentation. Every right — MIME defaults, provider declarations, autostart approvals,
instances — keys on the variant, because a grant belongs to the copy that earned it. A wrong merge
therefore shows two applications as one; it can never hand one copy's permissions to another.
`AppIdentity1.ResolveWindow` returns the variant for the same reason: a launch is a launch of one
particular copy, and the cgroup records which.

Trust is derived from provenance and never declared. A `System` variant is owned by something with
administrative rights over the machine; a `User` variant is a file the person's own account can
rewrite. A `User` variant may not take an exclusive provider role, and when a desktop id's
provenance moves from System to User, the provider declarations and autostart approvals it had are
dropped, with a history event saying so.
Native manifest files acquire provenance in the loader: canonical system source paths under
`/usr/share/templaros/apps` or `/etc/templaros/apps` establish NativeManifest origin, user sources
establish NativeUserManifest origin, and other overrides remain Unknown. JSON cannot supply its
own origin, trust, update owner or authoritative source path. The loader preserves native
`provides` metadata, which the installed-capability pass registers only for System variants.
See [provider source ownership](providers.md) for explicit isolated-guest discovery configuration.

Variant ids are stable except Flatpak exports, which are `flatpak.<X-Flatpak id>` rather than
`desktop-import.<desktop id>`. Old ids resolve through the `app_variant_aliases` table; nothing
stored elsewhere is rewritten, so a rolled-back binary reads the rows it wrote.

## Explicit workloads and governor resource identity

`templar run --class background-compute -- cargo build` admits a distinct child into an owned
`background.slice` scope before exec. `--class interactive` declares interactive intent. This is
opt-in: ordinary terminal commands retain their existing launch behavior; no shell hook moves the
terminal. Native argv, PTY and process group survive the gate, and the CLI propagates the command's
exit status.

`org.templaros.Workloads1` validates the actual unique sender's same-UID direct child, pins that
process with a pidfd, and passes systemd `PIDFDs`, not a recyclable numeric PID. Admission succeeds
only after observing the child's real `ControlGroup` and proving the caller is outside it.
`WorkloadId` is separate from the optional spawning `InstanceId`; an unresolved parent stays
unknown rather than inheriting a guessed application name. Durable records are restored only for
populated kernel scopes and removed after departure; an unreadable scope is not an absent scope.

The governor's `ResourceId` addresses a cgroup, while typed metadata distinguishes native
applications, explicit workloads and observed Flatpak scope claims. `Resources1.Targets`,
`ResolvePid` and `ExplainTarget` expose the generic view. The shipped `Report` remains
application-only. A workload or Flatpak claim is never converted into a canonical `AppId` grant.
Native identity/advisory policy is not sandbox confinement.

Compositor enrichment resolves actual peer PIDs off the render thread and ORs foreground/visibility
across all window-owning processes in a resource. Closing a workload's last window clears window
state without forgetting still-running headless work. Membership and producer-owner changes wake
the observer even while the compositor is idle. Background-compute defaults do not inherit the
terminal's foreground state or become disposable merely because they are headless. Unrelated
protection/checkpoint hints do not establish window visibility.

Native validation used an isolated session bus and the actual user systemd manager: pidfd-backed
sibling scope admission, kernel membership/PID resolution, unknown-parent reporting, window
enrichment/departure, two-PID foreground/visibility aggregation, idle producer restart without a
new window event, exit 7 propagation, unchanged parent cgroup, unchanged owned-scope OOM control
in observe-only, and durable-record reaping were observed. An actual interactive bash PTY stopped
with Ctrl-Z, resumed the original process group with `fg`, and returned 130 after Ctrl-C. Non-UTF8
executable names and argv bytes survived the native gate. A delayed first tick after producer
name acquisition woke retained focus through the new owner's membership stream.
An additional bounded native pressure run observed background/foreground/protected ranking,
real cgroup high events and tmpfs accounting, unchanged observe-only controls, and exact
owned-scope termination with the parent alive. Workloads remained non-disposable.
Installed-session acceptance remains separate; no installed service was restarted.
