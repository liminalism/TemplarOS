# distro/policy — system configuration the distribution owns

| Dir | What |
|---|---|
| `systemd/user/` | slice weights (`session.slice`, `app.slice`, `background.slice`) and protection drop-ins for `pipewire`/`wireplumber` |
| `systemd/system/` | system-level slice protection for `user.slice` vs `system.slice` |
| `sysctl.d/` | VM tuning matching zswap-backed swap |
| `tmpfiles.d/` | zswap parameters, `/run/templaros`, state dirs |
| `kernel/` | kernel command line fragments (zswap, quiet) |
| `udev/` | rules (NVIDIA runtime PM for hybrid mode) |
| `polkit/` | `org.templaros.host.policy` actions + rules |
| `dbus/` | system-bus policy for `org.templaros.Host` |
| `environment.d/` | `XDG_CURRENT_DESKTOP=TemplarOS` and friends |
| `apparmor/` | profiles for the daemons (`templar-hostd` wraps apt as root) |
| `coredump/` | systemd-coredump on, upload tools off; history ingests crashes |
| `pipewire/` | WirePlumber defaults |

Everything here is installed by packages (`debian/*.install`), never copied by hand.
