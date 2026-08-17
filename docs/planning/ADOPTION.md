# What the scaffold adopted from the planning documents

Everything in `01-initial-shape.md` and `02-subsequent-shape.md` is adopted, with these refinements
(each an ADR):

| Planning docs | Scaffold |
|---|---|
| `appd`, `associationd` (…"probably part of appd"), plus providers/history/settings/projects | one daemon `lege-sessiond`, separate library crates (ADR-0001) |
| `resourced` | separate protected process `lege-resourced`, self-describing unit names so it needs no D-Bus for emergencies (ADR-0002) |
| `updated`, driver authority, storage reserve, `locationd`/`timed-policy` | `lege-hostd` for the root parts; timezone/location policy in sessiond (ADR-0003) |
| "boot the candidate with a watchdog … roll back automatically" | systemd boot assessment + system-scope `lege-boot-health` + btrfs deployments (ADR-0004) |
| `org.example.*` placeholders | `org.legeos.*` (ADR-0008) |
| `systemd-run` launcher | kept for the prototype; `StartTransientUnit` next (documented in `lege-appd::launcher`) |
| workspace `desktop-os/{crates,services}` | this repository layout (`ARCHITECTURE.md`) |

Added, consistent with the docs: `lege` CLI + `lege doctor`, `lege-client` SDK, single portal backend,
`interfaces/` as single source of truth, `lege-hwdetect`, power/location domains, property tests, one state
store, notifications-as-history, autostart via sessiond, `distro/` (base.toml, seeds, metapackages, mkosi
image, apt, policy, branding, release, hardware, deploy, forks), `debian/`, `installer/`, i18n/a11y homes.
