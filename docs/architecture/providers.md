# Providers and background permissions
Exclusive roles (`ProviderRole::ALL`): exactly one selected provider may autostart. Background permissions
(login autostart, continuous background, network while hidden, global shortcuts, screen capture, input
monitoring, notifications, removable media) are declared, approved, and shown in Settings with the reason
each background app is running.

## Installed claims and source ownership

The resolver combines durable `Providers1.Declare` declarations and explicit selections with
capabilities rebuilt from the installed application catalogue. Desktop entries record
`X-TemplarOS-Provides=` as provenance claims; native manifests use `provides` entries with role
and priority. Both pass the existing System-trust gate. Importing an application does not grant
background consent: login autostart still requires approval as well as selection for its role.

Native-manifest loading derives origin, trust, update owner, executable and source path from
the actual source, not self-reported JSON provenance. Resolved files in
`/usr/share/templaros/apps` or `/etc/templaros/apps` are native system manifests; user manifests
and unknown override directories cannot acquire exclusive roles. A symlink into an unknown
source does not promote a native manifest.

Startup registers catalogue capabilities after native and application import. Autostart may
import an application absent from that initial catalogue, so it registers installed capabilities
again before planning and emits provider changes. Application rescans rebuild installed claims
while retiring only their own application-directory sources: an imported autostart source stays
installed and retains its declarations. Installed claims are not persisted as frozen grants.
Removal or changed metadata retires catalogue-owned claims on refresh, while an explicit variant
selection remains durable and may become stale. Native manifests are reloaded at daemon startup;
the desktop watcher does not rescan native JSON.

## Isolated guest configuration

`--no-default-discovery` disables the default native/application scan, not provenance or consent
checks. `--application-dir /opt/payload/applications` imports ordinary applications with Unknown
origin; a maintained package's desktop file copied there cannot nominate an exclusive provider.
Likewise, `--manifest-dir /opt/payload/manifests` is not a trust grant, even if JSON declares a
System origin.

For an administrator-controlled guest, install the real maintained desktop file in root-owned
`/usr/share/applications` and the native manifests in root-owned `/usr/share/templaros/apps`
(or `/etc/templaros/apps`), then point the explicit arguments at those directories. Keep real
executables, aliases and variant IDs unchanged. For example, Kitty's native `provides` entry
may declare `{"role":"terminal","priority":100}`, and the maintained nm-tray entry's
`X-TemplarOS-Provides=network-status-ui` nominates its imported variant at priority 50.
Selection remains keyed to that exact variant, not its logical application or executable.
Provider registration does not alter the original Exec template or nm-tray's `--popup` launch
support. Actual launch/resume acceptance requires exercising the real guest; source registration
alone is not runtime qualification.
