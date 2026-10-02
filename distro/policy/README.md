# distro/policy — system configuration the distribution owns

| Dir | What |
|---|---|
| `systemd/user/` | slice weights (`session.slice`, `app.slice`, `background.slice`) and protection drop-ins for `pipewire`/`wireplumber` |
| `systemd/system/` | system-level slice protection for `user.slice` vs `system.slice` |
| `sysctl.d/` | VM tuning matching zswap-backed swap (`60-templaros.conf`); hang evidence — SysRq dumps, hung-task/lockup backtraces, hard-lockup panic, `panic=10` (`60-templaros-hang-evidence.conf`) |
| `tmpfiles.d/` | zswap parameters, `/run/templaros`, state dirs |
| `kernel/` | kernel command line fragments (zswap, quiet) |
| `udev/` | rules (NVIDIA runtime PM for hybrid mode) |
| `polkit/` | `org.templaros.host.policy` actions + rules |
| `dbus/` | system-bus policy for `org.templaros.Host` |
| `environment.d/` | `XDG_CURRENT_DESKTOP=TemplarOS` and friends |
| `apparmor/` | profiles for the daemons (`templar-hostd` wraps apt as root) |
| `coredump/` | systemd-coredump on, upload tools off; history ingests crashes |
| `journald/` | persistent journal synced every 15 s, so a hard reset keeps the minutes before it |
| `pipewire/` | WirePlumber defaults |

Everything here is installed by packages (`debian/*.install` for the archive build,
`[package.metadata.deb]` assets for the Stage-1 `cargo deb` packages), never copied by hand. The
evidence files (coredump, journald, hang-evidence sysctls) ship with `templaros-hostd`;
`scripts/freeze-evidence-setup.sh` puts the same files at the same paths on a machine installed
before they did, and collects the previous boot's system journal (`templar doctor freeze` is
the unprivileged half).
