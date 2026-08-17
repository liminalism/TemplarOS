# Integration tests (harness skeleton)

Planned harness: start a session bus, `lege-sessiond --manifest-dir tests/fixtures/manifests`, `lege-filed`,
`lege-resourced --once`, then drive `lege` against them; later a nested compositor and `mkosi vm` runs that
execute the representative load (browser tab set + Rust build + cross-drive copy + video + deliberate
pressure + monitor hotplug) and assert pointer/switching/audio/overlay/terminal stay usable.
`run.sh` currently runs the in-process smoke tests only.
