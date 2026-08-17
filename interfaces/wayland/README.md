# Wayland protocols (private)

* `lege-shell-v1.xml` — commands only the trusted, compositor-launched shell may issue plus sanitized
  toplevel data (canonical app id / instance id / verified). Listing itself uses upstream
  `ext-foreign-toplevel-list-v1` / `cosmic-toplevel-info`.
* `lege-recovery-v1.xml` — the compositor-core-owned pressure/recovery overlay.

Single copy lives here; the compositor fork and `lege-comp-integration` generate bindings from these files.
Typed layer numbering matches `crates/lege-window/src/layers.rs`.
