# Application identity (`lege-appd`)
One canonical `AppId`; aliases are evidence. Verified identity = PID's cgroup is a registered (or
self-describing) launch unit. Confidence: unknown / claim-only / verified-launch / verified-launch-claim-mismatch.
Compatibility fallback accepts only a *unique* claim match. Manifests: `/usr/share/legeos/apps`,
`/etc/legeos/apps`, `$XDG_DATA_HOME/legeos/apps`. Desktop entries without a manifest are imported
(`lege-appd::desktop_entry`), keyed per origin namespace so a Flatpak export and a distribution
package sharing a desktop id stay two applications instead of one silently replacing the other.

## Variants and logical applications
An `AppId` names a **variant**: one installed copy, from one origin. A person sees one Firefox; the
system may hold the Debian package, the Flathub build and a `.desktop` file they wrote themselves.
`AppProvenance` (`crates/lege-types/src/variant.rs`, `interfaces/schemas/app-variant.schema.json`)
records where a copy came from — origin, trust tier, source path, package ref, version, launch
mechanism, update owner — and `crates/lege-appd/src/variants.rs` groups copies under a
`LogicalAppId` on evidence: a declared logical app or alias, a shared desktop-entry id across
origins, a flatpak id named in another's aliases, a shared `StartupWMClass` *and* name, or a shared
resolved executable. Minted logical ids are persisted, so they survive an uninstall.

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
