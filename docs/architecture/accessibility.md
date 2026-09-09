# Accessibility invariants

What holds today, enforced by gates and tests:

- The recovery overlay, window shelf, "show every window" and "go home" (`Ctrl+Alt+H` on any
  keyboard, `Super+D` where there is a logo key) are keyboard-reachable without the shell.
- The Home Field is keyboard-complete with first-class pointer parity: every declared action
  with a chord also has a menu or region route (`xtask check-actions` fails the build
  otherwise), and the F1 help sheet is generated from the same action manifest.
- Keyboard focus has a genuinely distinct visible indicator, and reduced motion is a respected
  setting.
- Everything the shell shows is available through `templar` too.

What is **not yet implemented** — deliberately deferred, not shipped:

- **No assistive-technology bridge.** The shell paints with its own rasterizer and registers no
  AT-SPI objects: no name, role or state is exposed for any tile, drawer row or widget region,
  and no a11y bus is spoken anywhere in the workspace. `opt-a11y.seed` installs Orca,
  at-spi2-core and speech-dispatcher so the stack is present for when the bridge exists, but a
  screen reader meets a silent desktop today. Do not describe TemplarOS as screen-reader usable.
- **No high-contrast variant.** No high-contrast token, theme or setting exists in the shell.

One input method (`input-method` role).
