# Package selection

`selection.toml` is the **one** hand-written package list in this repository. Everything else here
and in `distro/image/mkosi.conf.d/` and `distro/metapackages/` is generated from it:

```sh
cargo run -q -p xtask -- seeds            # render every generated artifact
cargo run -q -p xtask -- seeds --check    # fail if one has drifted (also run by check-packaging)
cargo run -q -p xtask -- seeds closure    # what the selection resolves to, on this machine, now
```

| Generated | From |
|---|---|
| `<layer>.seed` | one flat name list per layer, for readers that want names only |
| `flatpak-refs.list` | `<remote> <ref>` per line — the refs apt cannot install (ADR-0011) |
| `../image/mkosi.conf.d/10-packages-base.conf` | the layers with `stage = "base"` |
| `../image/mkosi.conf.d/50-seeds.conf` | the layers with `stage = "seeds"` and `image = true` |
| `../metapackages/*/control.in` | the `[[metapackage]]` stanzas |

Never edit a generated file: `cargo xtask seeds` overwrites it and `cargo xtask check-packaging`
fails on it. Add the package to `selection.toml` instead, with a reason.

## Layers

`bootstrap` (a machine that boots and can install the rest of itself), `hardware`, `graphics`,
`session`, `desktop`, `browser`, `media`, `dev-rust`, `dev-cuda`, and the optional `opt-a11y`,
`opt-ime`, `opt-bluetooth`. `nvidia-open` and `nvidia-proprietary` are *variants* of one group:
exactly one is selected, by `lege-hwdetect`, and neither is in the dev/VM image.

Every direct package carries a one-line `reason`. A `role` (`terminal`, `browser`,
`input-method`, `secret-service`, `gpu-driver`, …) is exclusive — two selected packages claiming
one role is an error, which is how "two input methods fighting over one protocol" fails here
rather than on a user's desktop.

Excluded on purpose (ADR-0011), and rendered as `Conflicts:` on `legeos-desktop`: `snapd`,
`apport`, `whoopsie`, `ubuntu-report`, second desktop stacks.

## The closure report

`seeds closure` prints JSON. Its `resolution` field is the part to read first:

- `resolved` — apt has indexes for the codename `distro/base.toml` declares. Only then are
  `transitive`, `installed_size_bytes`, `recommends` and `missing` claims about anything.
- `foreign-release` — apt answered, but for another release. Indicative only.
- `unavailable` — no archive index at all. An empty `missing` list means *nothing was asked*, not
  that nothing is missing. This is the state on a development host with no Ubuntu 26.04 indexes.

`observed_at` (ISO-8601, UTC) and `resolved_against` record when and against what; the snapshot pin
lives in `distro/base.toml` (`snapshot_url`, `snapshot_serial`) and empty means unpinned.

## The `--no-recommends` experiment

The deployment path already installs with `--no-install-recommends`
(`cli/lege/src/dev/install.rs`); the image does not. `[policy].recommends = "on"` records that,
and `seeds closure` reports `recommends.no_recommends_experiment.would_drop` — the packages that
only a `Recommends:` pulls in — by simulating the install both ways. **This is a report, not a
policy.** Turning recommends off for the image is a decision to take with that list in hand: it
typically drops firmware, language packs, PolicyKit agents and portal backends that nothing
`Depends:` on but a working desktop expects. Nothing here applies it.
