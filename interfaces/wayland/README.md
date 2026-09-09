# Wayland protocols (private)

* `templar-shell-v1.xml` — commands only the trusted, compositor-launched shell may issue plus sanitized
  toplevel data (canonical app id / instance id / verified). Listing itself uses upstream
  `ext-foreign-toplevel-list-v1`. Version 2 adds `reveal_home` / `home_revealed` (go home), and per
  toplevel the `ext_foreign_toplevel_handle_v1` `identifier` (the key a window preview is filed under),
  `geometry`, `maximized` and `set_maximized`. Both peers live in this repository and bind the lower of
  what each speaks; every `since="2"` event is guarded on the bound version.
* `templar-recovery-v1.xml` — the compositor-core-owned pressure/recovery overlay.

Single copy lives here; the compositor fork and `templar-comp-integration` generate bindings from these files.
Typed layer numbering matches `crates/templar-window/src/layers.rs`.

`cargo xtask check-interfaces` covers `interfaces/dbus/` only; drift between these files and the two
generated modules (`compositor/templar-comp/src/protocols.rs`, `shell/templar-shell/src/protocol.rs`) is
caught by those modules failing to compile, not by a check of their own.
