# Application identity (`templar-appd`)
One canonical `AppId`; aliases are evidence. Verified identity = PID's cgroup is a registered (or
self-describing) launch unit. Confidence: unknown / claim-only / verified-launch / verified-launch-claim-mismatch.
Compatibility fallback accepts only a *unique* claim match. Manifests: `/usr/share/templaros/apps`,
`/etc/templaros/apps`, `$XDG_DATA_HOME/templaros/apps`. Desktop entries without a manifest are imported
(`templar-appd::desktop_entry`), keyed per origin namespace so a Flatpak export and a distribution
package sharing a desktop id stay two applications instead of one silently replacing the other.

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

Variant ids are stable except Flatpak exports, which are `flatpak.<X-Flatpak id>` rather than
`desktop-import.<desktop id>`. Old ids resolve through the `app_variant_aliases` table; nothing
stored elsewhere is rewritten, so a rolled-back binary reads the rows it wrote.
