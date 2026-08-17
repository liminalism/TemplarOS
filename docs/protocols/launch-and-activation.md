# Launch, identity and activation

1. `Associations1.ResolveFile(path)` → `(app_id, mime, reason)`; empty app → chooser UI.
2. `Launcher1.Launch(app_id, args, activation_token)` → `appd` starts
   `app-legeos-<AppId>@<InstanceId>.service` in `app.slice` (`systemd-run` now, `StartTransientUnit` later),
   env `LEGEOS_APP_ID` / `LEGEOS_INSTANCE_ID`, `OOMPolicy=kill`, accounting on.
3. First window: compositor reads the peer PID → `lege-window::WindowService::on_toplevel_mapped` (geometry
   repaired, identity from cache or `UNKNOWN`) → `lege-comp-integration` asks `AppIdentity1.ResolveWindow`
   asynchronously → `update_identity`. Dock groups by canonical AppId; restore records InstanceId;
   `lege-resourced` watches the same cgroup.
4. Activation ≠ identity: `appd` says *who owns* the window; the compositor's xdg-activation policy says
   *whether focus may be stolen* (`ActivationGrant`, recent-input evidence).
