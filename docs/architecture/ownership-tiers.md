# System, State and Work ownership

The executable declarations are `distro/deploy/layout.toml` and
`distro/deploy/components.toml`. `just check-ownership` checks component/subvolume
owners, write surfaces, package asset destinations, declared unit writable paths,
state-bind boundaries and pinned decision references. It resolves references through
`akr --format json get <pinned-ref>` and requires an active, current, fresh normative
decision. Missing AKR tooling or proposed/stale/retired/superseded authority fails
with a reconciliation diagnostic. Proposed decisions remain source-candidates, not
installed/release authority. This is a **declaration aid, not runtime confinement**;
it neither proves every indirect write nor makes an AppArmor complain profile enforcing.

Native source verification on 2026-10-06 checked 13 components, five subvolumes and 31 path
domains. The same checker, compiled into a throwaway native command with real Cargo metadata,
accepted an owned baseline and rejected undeclared Work writes, missing owners, missing tiers,
contradictory unit writable paths, invalid decision exceptions, Work-tier package payloads and
undeclared package-writing updaters. These disposable declarations did not change the host.
The ownership decision's support was repointed to the current layout-driven production source
observation, and every source exception pin now uses `three-ownership-tiers/2`; its policy is
unchanged. Actual Btrfs/ESP boot and rollback acceptance remain separate gates.

The subsequent native publication audit checked 14 components and 32 path domains:
the standalone Displays package is in the build matrix and owns only its
`%h/.config/templar-displays` profile directory. It is a relaunchable application,
not a second compositor service; its X11 RandR route does not require a TemplarOS
compositor or session-policy daemon. Compositor-owned display profiles remain separate.

## Tiers

| Tier | Authority | Persistence and rollback |
| --- | --- | --- |
| System | Installed packages and the privileged host deployment/account writers | Root-paired OS, `/usr`, `/etc`, dpkg/APT databases and APT cache; boot selects that root's own relative Btrfs identity. |
| State | The named service/application or reviewed external runtime owner | Narrow directory binds backed by `@var`, host journal/registry, device configuration, caches and application profiles. Reader contracts and explicit migration govern compatibility. |
| Work | The user, with application operations explicitly authorized by the user | `/home`, optional `/data` and user documents/projects/media. Package payloads never install here and OS rollback never restores a Work snapshot. |

Most-specific declarations classify nested State below a user's home. A filed
exception for user-authorized Work operations is tier-scoped: `%h/**` is not a grant
to overwrite another service's State. Shared SQLite/container/bookmark surfaces and
legacy overlay deployment exceptions carry explicit pinned decision references and
reasons; no blanket wildcard changes ownership.

## Native deployment boundary

`@var` mounts at `/var/lib/templaros/state`, **not `/var`**. Only the declared
directory domains are bound from it. In particular `/var/lib/dpkg`, `/var/lib/apt`
and `/var/cache/apt` stay in the selected System root; atomic machine/account file
writers are not broken by per-file binds. The native snapshotter verifies that the
package databases/cache share the source root's mount identity and that the
mounted candidate namespace matches the declared relative subvolume namespace.
Legacy blanket-`/var` roots require reviewed offline migration; automatic resume
or candidate staging must refuse rather than guess which package database belongs
to the root.

GPU mode is persistent State at `/var/lib/templaros/hardware/gpu-mode`; offline
and candidate preparation preserve only the old GPU mode when the new target
is absent. The driver version lock remains canonical deployment-paired System
at `/etc/templaros/hardware/version-lock.toml`; it is not an @var bind or a lock
copied from another root's package set. Explicit authenticated lock repair is a
declared live-System exception on RW only; RO repair must refuse until a real
candidate repair operation exists, never suppress mismatches.
Before candidate ESP publication/sealing, hostd derives that root's lock from its
selected matching kernel/initramfs and readable paired dpkg metadata. Retained
other-ABI module packages are not mistaken for the selected ABI's baseline.


Native `templar dev deploy/install` queues a pinned hostd candidate. Its return is
not installation or promotion. APT download/configuration/installation runs inside
the writable candidate's transient `RootDirectory`, with private API/device/PID
mounts, no live service-manager bus, read-only public archive inputs (never the
archive signer), bounded lifetime and restricted kernel/device authority. Ubuntu
maintainer scripts still run; a failing script fails the candidate, not a fake
boot-hook substitute. Boot files are copied to the owned ESP namespace before an
opt-in read-only candidate is sealed. Hostd serializes package staging, retention,
boot selection and password maintenance.

Transactions persist mounted paths, relative subvolume identity, ESP identity and
root mode. A restart refuses to reinterpret an already-staged root under a changed
layout. Host journal/registry schema 1 accepts the legacy representation, retains
pre-write bytes as `.pre-schema-1`, and refuses unknown future schema without
rewriting it. The persistent State marker and both root reader windows must agree.
Trial promotion publishes durable registry membership before terminating the
recoverable journal; interrupted publication can repeat without duplicating roots.
A root switch does not restore application profiles or Work; it is not a generic
profile-format migration. Native developer `--snapshot-state` is refused rather
than pretending a profile rewind is safe.

## Owned self-password maintenance

`templar account password` sends a bounded UTF-8 password through an immutable,
sealed, CLOEXEC memfd to `org.templaros.Host.Accounts1.ChangeSelfPassword(h)`.
Hostd derives the sender UID from D-Bus (local UID >= 1000), has no caller-supplied
username/UID argument, requests interactive polkit self-authentication and checks
the sender again afterwards. The payload is 1..1024 bytes; NUL/CR/LF are refused;
colon is permitted. Plaintext is not argv, JSON, a temporary file, a journal field
or command output.

The real writer executes native `chpasswd`/PAM in a disposable writable System
snapshot inside an owned private mount namespace. It preflights every registered,
fallback and current candidate account database, requires compatible reader
contracts, and propagates only the authenticated caller's encrypted shadow
password/last-change row. Account structure, other users and unrelated shadow
fields cannot silently migrate. Native `passwd`, NSS and the existing fingerprint
rules are not replaced; direct default-RW account changes that leave roots
incompatible cause subsequent paired operations to refuse, never downgrade them.

Before durable intent, the writer establishes readable, root-owned non-secret
`/var/lib/templaros/account-maintenance/active.json` in a separately bound State
directory. The account-only `pam-auth-update` profile is additive to `common-account`;
it does not change auth/password/fingerprint rules. Every PAM service including
that standard account stack is gated, regardless of real UID (login/su/polkit),
and native login also requires the State bind to be mounted. A malformed marker,
untrusted parent or unreadable state fails closed. Custom PAM services must retain
that common-account inclusion; they are not bypass permissions.

The root-only journal records encrypted old/new shadow bytes, metadata/extended
attributes, original subvolume properties and exact scratch/root identities.
Preparing intent owns scratch cleanup; prepared intent recovers old credentials;
committed intent rolls forward new credentials. Both writing and recovery use
shadow-utils-compatible POSIX password locks; recovery checks every root before
replacing any shadow. Unjournaled concurrent credentials are refused, never
overwritten. Temporary properties are restored on failure, without making the
user's live root mount writable. The public marker requires its dedicated State
bind to be mounted before native maintenance starts.
The public gate is removed only after writer/recovery cleanup completes. Boot recovery runs
when either intent or marker exists; a lost marker is recreated before repair and
an orphan marker is cleared only with the writer lock held and no durable intent.

## Adoption and owner proof

Writable roots remain the default. `installer/provision.sh --read-only` and
`TEMPLAROS_READ_ONLY=1 just image build` are explicit opt-ins, not a claim that a
physical RO boot/password/update cycle has passed. Promotion/adoption requires an
owned disposable Btrfs/ESP run and physical GRUB/systemd-boot trials: paired package
DB/cache, matching boot files, actual selected UUID/subvolume/mode, known-good
fallback and interrupted-phase resume; changed password across trial/fallback;
kill/power interruption before/after prepared/committed intent; no secret argv,
files or logs; original RO properties and live root mount restored; PAM password
and fingerprint behavior preserved. Profile/schema incompatibility and malformed
recovery state must refuse without modifying Work. The installer checklist records
those owner-only proofs; source tests do not replace them.

## Native device UI ownership

NetworkManager and BlueZ remain the applied-state, device-policy and polkit
authorities. `org.templaros.Network1` and `org.templaros.Bluetooth1` expose actual
upstream inventory and bounded effects on `org.templaros.Session`; their Settings
controllers keep one caller connection and serialize event refreshes with writes.
Both upstream and UI signal matches are installed before initial readback. There
is no periodic refresh or desired-state success substitute. Producer replacement
invalidates the owner epoch, pending prompts and secret drafts; the UI also drops
its old unique caller before subscribing to the replacement.

Native prompts require an explicit unique-caller UI lease. Tokens and lease
identities are unpredictable, replies are one-shot, and prompt/display lifetimes
are bounded to 120 seconds. Page leave/caller loss revokes replies before waiting
for in-flight effects or cleanup. Failed/timed-out cancellation, discovery release
or unregister retains cleanup ownership and reports uncertainty instead of
claiming the upstream effect disappeared. BlueZ pairing has a 180-second deadline
and retains a pending device if cancellation is not confirmed. Native discovery
ownership is distinct from BlueZ's global `Discovering` value.
Registration and effects target the exact captured upstream unique owner, preventing
a producer replacement between registration and Pair from falling through to another
agent. Issued NM activation RPCs survive the UI's shorter wait deadline so a late
existing-profile reply supplies the actual owned active path; cancellation never
guesses existing-profile ownership from a shared UUID. An unresolved transport
outcome remains explicit and blocks a new native activation until reconciled.
Only an explicit method refusal from that captured producer confirms that an issued
activation/discovery request failed. `NoReply`, timeout/disconnection results and
errors from the bus itself do not discard its cleanup ownership. Once prior BlueZ
cleanup is positively confirmed, the succeeding lease clears the obsolete cleanup
error rather than presenting an already-resolved failure on the new page.

The native NM agent handles only the explicit personal connection operations
claimed by that lease. SSID identity is the original 1..32 byte vector, not its
lossy display label; hidden SSIDs can be entered as complete hexadecimal byte
pairs. Enterprise/VPN/legacy configuration remains delegated. Unknown profiles
return `NoSecrets` to permit the existing provider, while an actual user rejection
returns the terminal
[`org.freedesktop.NetworkManager.SecretAgent.UserCanceled`](https://lxr.kde.org/source/frameworks/networkmanager-qt/src/secretagent.cpp)
wire error. Existing system-owned PSKs are not solicited by the native agent or
silently migrated into NM keyfiles.

The declared gnome-keyring `org.freedesktop.secrets` service remains the durable
credential authority. Locked/missing collections, lookup errors and unconfirmed
saves are separate refusals; session-only credentials are an explicit alternative,
not a fallback. Secret Service transactions and Close are bound to the actual
unique provider which replied, so a replacement cannot receive a stale session's
credential. Passwords/PINs/codes are excluded from inventory, Changed and command
Debug. UI transport uses fully immutable sealed memfds; owned secret buffers are
zeroized on reply, replacement, rejection and page leave. Required NM/BlueZ agent
wire values are typed and borrowed while encoding, not ordinary owned-string
copies. PixelKit masked fields retain clipboard-copy/cut refusal and no accessible
Value; sensitive view owners explicitly clear their field storage on leave/drop.

BlueZ's application agent is registered on the same system connection that calls
Pair, and never requests the default-agent role. PIN, passkey, comparison,
authorization and display callbacks authenticate the actual current BlueZ owner.
Comparison/authorization require the user; no trusted state or service approval is
granted implicitly. Display codes are hidden until explicit reveal and transferred
only through the caller/token-bound FD method.

The shipped Qt `NetworkStatusUi` default and compatibility tray remain unchanged
until owner acceptance. Private-bus authentication/prompt/typed-wire tests and
view-focus/FD boundary tests are source fixtures, not radio/keyring acceptance.
Owner proof still requires open/WPA/WPA3/hidden byte-SSID connections, bad-password
and locked-keyring behavior, saved reconnect/forget/autoconnect, cancellation and
producer/caller replacement without a second prompt; and real keyboard, phone and
headset pairing with all agent variants, rfkill/firmware failure, owned discovery
cleanup and Sound routing. Mixed scale, keyboard focus, clipboard and masked
accessibility must be exercised on the native desktop before any provider cutover.

### Isolated native device proof (2026-10-06)

The retained `.agent/scratch/native-device-proof/` source-import harness was built
with `cargo build --offline --manifest-path .agent/scratch/native-device-proof/Cargo.toml
--target-dir target/native/ubuntu-26.04`, then run using
`/usr/bin/python3 -I .agent/scratch/native-device-proof/run.py`. Its final run exited
zero; `evidence.txt` records every daemon/client command and the owned root
`owned-5qhnm7lf`. Bubblewrap supplied mapped UID 0 plus private network/PID/mount
namespaces, empty `/sys`, private `/dev`, HOME/XDG, `/etc`/`/var`/`/run` and both
buses. Private fixture buses disable AppArmor mediation and NM's owned config
disables polkit; this does **not** exercise installed authorization or alter the
product's policy. Only exact fixture child processes were signaled and reaped.

Genuine `/usr/sbin/NetworkManager` 1.54.3 owned `:1.2` (namespace PID 5).
`Network1`'s real typed client observed only unmanaged loopback (device type 32),
state 20, no Wi-Fi APs or existing profiles. The daemon logged registration of
`:1.4/templaros-native-settings/0`. Wrong UI/agent callers, fabricated prompt
tokens, missing Wi-Fi paths and page-closed mutation were rejected. An actual
unsaved secured profile's `GetSecrets` returned NM's
`org.freedesktop.NetworkManager.AgentManager.NoSecrets` without a native prompt;
the profile was deleted. UI caller loss allowed a successor lease; stopping the
exact daemon revoked ownership and advanced the producer epoch. The daemon also
reported plugin-owner checks and namespace link/sysctl permission refusals, so
this proves native registration/readback/negative lifecycles, not radio effects.

Genuine `bluetoothd` 5.85 was launched against the same private system bus but
exited 1 with `Failed to access management interface` and
`Adapter handling initialization failed`. `Bluetooth1` returned unavailable
readback and acquisition failed. No native pairing callback or device operation
was exercised.

A separately labeled, zero-device application `AgentManager1` counterparty
(owner `:1.6`, exported application agent `:1.7`) exercised the real Bluetooth1
API and Agent1 callbacks over those buses. It confirmed caller/token/one-shot
checks, immutable offset-independent bounded secret memfd transport, mutable and
oversized/PIN-bound FD rejection, no codes in inventory, explicit display reveal,
cancel/reject/page-close revocation, retained unconfirmed unregister ownership,
successful cleanup before successor acquisition with stale-error removal, and
producer-owner loss. This is controlled protocol evidence, **not bluetoothd or
physical pairing**. Wi-Fi/WPA/WPA3/hidden SSIDs, authentication, captive portals,
installed polkit, rfkill, firmware, USB, actual Bluetooth peripherals and Sound
routing remain the owner-only gates above; the managed provider remains default.


## Native power policy ownership

`Power1` now separates stored battery/mains profiles from the actual selected source,
native observed timeout, per-field capability/armed/error and scoped inhibitor state.
Its single serialized sessiond owner consumes compositor activity samples and real
input serial transitions. Timers are one-shot policy deadlines, not another input
watcher, logind IdleHint clock or periodic status poll. Changed selected source/profile
re-arms through `SetIdleTimeout`, which resets the compositor's Instant epoch without
inventing user input, and only its native response confirms the applied timeout.

[UPower](https://upower.freedesktop.org/docs/UPower.html) supplies actual OnBattery,
LidIsPresent and LidIsClosed values after signal matches are installed. Missing system
producers, inactive native owners and inaccessible output readback remain unavailable,
not mains/DPMS-off/covered defaults. Dimming uses the existing Backlight owner and
actual readback; restoring remembered brightness is conditional on the same device,
scale and raw value still belonging to the dimmer, never overwriting a different
external brightness. No backlight is reported truthfully.

Lid handling owns a scoped logind `handle-lid-switch` block only while its actual
native/system handling is available. Docked is actual connected **and enabled**
external topology. Existing Ignore/Lock/Suspend/Hibernate encodings remain unchanged;
serde-defaulted awake-panel fields add Keep/ExternalOnly/AllowHeadless without making
stored profiles unreadable by the retained older lid-action reader. ExternalOnly
never disables the last output; AllowHeadless is explicit. Suspend/Hibernate retain
outputs for the security barrier. Reopen delegates saved-profile revalidation and
exact presented/readback completion to the compositor, not a second clamshell path.
An owned effect refusal is retained as an error, not permission to bypass the policy
via a fallback. On handling/lifetime loss scoped inhibitors and streams are released.

Automatic sleep emits LockRequested when necessary and requires a fresh native,
active, current-generation `protected_covered` observation immediately before the
logind effect. A saved Locked edge or merely requested lock never authorizes sleep.
Hibernate is advertised only through the actual CanHibernate result; automatic
effects require noninteractive authorization. The existing single PrepareForSleep
watcher feeds SessionEdges and publishes a retained-owner readiness witness only after
actual native match registration. Sleep capability and delay ownership require that
witness; stream/end/bus loss or owner drop release the scoped delay and refuse automatic
sleep. Its scoped sleep delay is otherwise released only after fresh coverage and
reacquired on resume/recovery. **This cannot guarantee cancellation of
externally initiated suspension:** logind proceeds after its finite
[InhibitDelayMaxUSec](https://www.freedesktop.org/software/systemd/man/org.freedesktop.login1.html)
even if frame coverage is still unconfirmed and the FD remains held.

Settings edits every idle/lid/awake-panel field through the same typed Power1
authority, with exact decimal-second drafts, reset, keyboard/IME controls, observed
results and stable focus independent of ordinary producer churn. Hidden-page leave
does not terminate global policy or erase backend capability/field errors. An inactive
source may be stored, but that never makes its policy selected or applied.

Source fixtures for deadline decisions, current-generation coverage, profile
storage/rollback shape and GUI drafts are not hardware acceptance. Owner proof remains:
actual dim/conditional restore, source changes/relogin, idle inhibitors, docked and
undocked lid transitions, explicit headless refusal/selection, lost/restarted owners,
coverage failures and resume with changed outputs. Physical suspend/hibernate, panel
readback and security presentation must be exercised on the reference hardware before
default promotion; no live power action was performed during this implementation.
