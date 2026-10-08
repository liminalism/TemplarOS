# Session
`templaros-session.target` + user units with protection values; `templar-session` entry; greetd + templar-greeter (ADR-0012);
autostart arbitrated by sessiond (one provider per exclusive role; `xdg-desktop-autostart.target` masked).

After registering its interfaces and claiming the session bus name, sessiond starts its
background watchers and session-edge delivery independently of best-effort host health
reporting. Both session-health and compositor-readiness reports have a five-second deadline
covering proxy initialization and the call. A missing or unresponsive host service is logged;
it cannot prevent application catalogue refresh or silently manufacture a successful report.

## Shared appearance and supported toolkit configuration

Native appearance uses the existing light/dark materials, named accent and Home reduced-motion
preference. Pure `templar-types` tokens feed one windowing-free `templar-ui` adapter.
Retained client observers subscribe before reading one atomic `Settings1.Inspect` snapshot and
read back on producer-owner changes, including restarts that emit no setting-edit signal.
Hosts invalidate paint caches, not field geometry, placements, navigation or pending forms.

`shell.appearance.third-party` is opt-in and false by default. When enabled, sessiond applies
only supported GTK3/GTK4 configuration keys; opt-out restores the journalled original keys
while preserving unrelated edits. Settings/Reset/Undo commit authoritative state before
publishing cache or configuration effects. A partial filesystem failure reports the committed
setting and applied paths; it does not claim that the operation was rolled back.
Mutation guards remain held until blocking writes finish, even if a caller cancels its future.
Native no-follow/version checks and atomic exchange prevent overwriting an intervening edit;
conflict recovery remains explicit.

Qt uses the installed GTK3 platform-theme plugins; this is supported future-launch integration,
not a custom Qt theme, arbitrary GTK/Qt accent propagation or live window reload.
The session bootstrap follows the existing user-manager environment lifecycle rather than
writing global profile or `environment.d` overrides. A shared user manager does not isolate
concurrent desktop sessions. GTK configuration changes affect that user's future GTK launches
and are therefore separately opt-in.

Owned private-bus/native-filesystem runs observed opt-out preservation, GTK3/4 application,
Undo/Reset restoration, no effect on SQLite commit failure, and truthful committed partial
failure under a real unwritable GTK4 directory. Producer replacement without `Changed`
failed the previous observer and passed the atomic owner-aware observer. Rendered GUI colors,
native clipboard/IME/Orca, installed-session appearance and live toolkit reload are separate
acceptance observations, not consequences of these service runs.


## Power policy ownership and evidence

`Power1` stores independent battery/mains profiles; `Inspect` separates those desired profiles
from the selected source, current producer capabilities, armed deadlines and owned inhibitors.
A selected-source write can persist successfully while returning an error because native
application was not confirmed. Reset removes the source override rather than inventing applied
state.

One serialized actor derives deadlines from compositor activity. The sole lifecycle
`PrepareForSleep` stream's registered/lifetime witness is required before acquiring a delay
inhibitor or authorizing automatic sleep. UPower loss withdraws selected automatic profile/lid
policy, but does not release a healthy login1/native/SleepWatch delay guard for external sleep.
Logind loss retires its FD; native/inactive-seat loss retires an idle guard. An external sleep
already awaiting protection retains its delay FD through native coverage loss/deadline expiry
until resume or lifetime loss. Expiration is recorded even when native readback is unavailable,
so a past barrier deadline cannot become a retry spin.
An accounting `Locked` edge is never a protected-frame authorization.

Native private-bus execution observed actual `PowerApi` storage/selection/reset behavior,
unavailable/inactive/missing-witness sleep refusal, uncovered barrier expiry, real pipe-FD
retention across UPower-only loss, acquisition without UPower, release on login1/native/witness/
actor loss, and conditional brightness restoration through actual `BacklightApi`/logind
transport confined to an owned sysfs-shaped directory.
An owned Unix peer was pinned by production `SO_PEERPIDFD` transport and rejected for lacking
the managed compositor cgroup. The actor fixture's explicit `from_socket` samples and logind
capability answers are controlled policy inputs: they do not establish physical authorization,
KMS protected presentation, actual suspension, docked lid behavior or resume with changed
physical outputs. Those remain installed/hardware gates. No host backlight was read or changed.
