# interfaces/ — the contracts

Everything two components agree on lives here, in one copy: D-Bus introspection XML (`dbus/`), private
Wayland protocols (`wayland/`), JSON schemas (`schemas/`). Code is checked against these by
`cargo xtask check-interfaces`. Changing a shipped signature means a new versioned interface, not an edit
— and `baseline.json` is what makes that a rule the build enforces rather than a rule people remember.

`baseline.json` records the D-Bus signature of every member of every shipped interface.
`cargo xtask check-interface-signatures` diffs `dbus/` against it: a **new** member passes (additive
growth on a shipped version is how this project already works — `AppIdentity1` gained `ListApplications`
without a bump), while a member whose arguments changed, or that disappeared, fails and names the
interface to add instead. After adding a member, re-freeze it with `cargo xtask interface-baseline` and
commit the result in the same change.

`cargo xtask check-wayland-versions` also checks private Wayland interface versions and negotiated
constants. The shell manager, toplevel list and toplevel interfaces must have **equal** versions,
because child resources inherit the manager's negotiated version. Versioned per-toplevel requests
must use a reachable manager revision (`1..=manager.version`), never a private child-only revision.
Historical requests retain their introduction version even after the manager advances.
