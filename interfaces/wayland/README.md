# Wayland protocols (private)

* `templar-shell-v1.xml` — commands only the trusted, compositor-launched shell may issue plus sanitized
  toplevel data (canonical app id / instance id / verified). Listing itself uses upstream
  `ext-foreign-toplevel-list-v1`. Version 2 adds `reveal_home` / `home_revealed` (go home), and per
  toplevel the `ext_foreign_toplevel_handle_v1` `identifier` (the key a window preview is filed under),
  `geometry`, `maximized` and `set_maximized`. Both peers live in this repository and bind the lower of
  what each speaks; every `since="2"` event is guarded on the bound version.
  Version 11 adds `focus_surface`: a visible shell popup borrows keyboard focus without changing
  client activation, MRU or Home reveal. A null surface releases it only while that shell
  connection still owns popup focus; hidden surfaces, other clients, lock and Taskfield are refused.
  Plain Escape reaches a focused shell popup before the underlying Home reveal.
  Version 12 adds `set_minimize_target` on the toplevel (the list and toplevel objects inherit the
  manager's bound version, so all three move to 12 together): the shell names the rectangle on
  its own Home surface (surface-local) where a window's card sits, and the compositor draws that
  window shrinking into it when shelved and growing back out of it when unshelved. A motion hint only
  — never geometry, layout, stacking or input; a null surface or an empty rectangle clears it.
  Version 13 adds `switcher_state`: active, selected window id and a native-endian u32 array of
  window ids in the compositor's frozen walk order. Only the trusted shell receives this snapshot;
  it paints a Presentation surface while key handling, focus, MRU and commit stay compositor-owned.
  `set_reduced_motion` shares the existing shell preference with compositor park/restore motion.
  Per-toplevel `claimed_app_id` is an unverified client display hint, not canonical identity;
  shells show it with a `?` mark and never grant capabilities or merge trusted app units from it.
* `templar-recovery-v1.xml` — the compositor-core-owned pressure/recovery overlay.

Single copy lives here; the compositor fork and `templar-comp-integration` generate bindings from these files.
Typed layer numbering matches `crates/templar-window/src/layers.rs`.

`cargo xtask check-interfaces` covers `interfaces/dbus/` only; drift between these files and the two
generated modules (`compositor/templar-comp/src/protocols.rs`, `shell/templar-shell/src/protocol.rs`) is
caught by those modules failing to compile, not by a check of their own.
