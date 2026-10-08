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

Native PixelKit hosts use the shared semantic graph/AccessKit route, Unicode text-field state,
visible focus, native IME caret positioning and a retained native clipboard worker. Modal graphs
exclude their background, and secret fields expose a named password-input role without a value.
Settings, Files, portal dialogs and the greeter use those foundations. The custom Home window
now attaches the same native AccessKit adapter: its omnibox, placed tiles, Files drawer controls,
browser locations/entries, context menus and rename controls publish names, roles, physical bounds,
focus and selection. Modal Home graphs replace background controls with a modal dialog root.
Native actions enter the existing input handlers; data/layout changes retire the old action targets.
Plain text fields expose actual TextRun children, grapheme byte lengths and caret/selection
metadata, enabling native AT-SPI Text/EditableText rather than a value-only named control.
SetValue (at most 1 MiB, rejected rather than truncated) and SetTextSelection enter the existing
input-first editor only for the live root/control and its current run. Password fields never
publish plaintext values/runs or advertise those edit actions, including when visually revealed.

Owned native Linux acceptance used private session/system/AT-SPI buses, genuine at-spi2 producers,
an owned dconf service and installed Orca. Orca discovered the real Home producers and processed
their named-control focus events; its debug presentation is evidence of reader traversal, not audible
speech or a connected braille device. Genuine fcitx5 XIM displayed a Unicode candidate at Home's
caret and committed `é`; a native clipboard round trip retained decomposed accents and an emoji
ZWJ sequence byte-for-byte. A separate Home Wayland journey removed `DISPLAY` from both Home
and clipboard-peer environments and used the compositor's actual data-control clipboard route.
That route requires a supported compositor; it is not a normal `wl_data_device` serial transport
for arbitrary compositors. No clipboard helper shim, desktop activation or physical device was used.
The native Home producer's GetText preserved the exact decomposed/CJK/ZWJ fixture, reported
13 Unicode scalars, and accepted real caret/selection/edit requests. After replacement, its
retired text target returned UnknownObject and could not edit the current field.

The same native consumer selected an owned file, opened its real modal rename graph, edited the
filename and activated Rename through AT-SPI; the file service produced the exact UTF-8 filename
and preserved its contents. Native Tab/Tab/Enter cancelled the next dialog without another rename.
Both file and group dialogs count coalesced Tab events before the input-first confirmation pass.

Fresh native winit density sources at 1/1.25/1.5/2 produced physical input widths
520/650/780/1040 on an owned 1600×1000 display. Final settled native Home frames at every
density show the actual indexed treemap and its measured-size generation/sequence caption.
Both standard input text and custom tracked role fonts grow: measured capital heights are
10/12/15/20 pixels, and the typed `alpha` first glyph is 7/9/11/15 pixels high. Native keyboard
input retained the exact text, Tab visited the Files tile and nested Recent control, Enter opened
the real browser, and Escape collapsed it at all four densities. The shared traversal visits tiles
without also advancing their legacy cursor; tile arrows and activation keep their existing handlers.
These are inspected live native pixels, not geometry-only loading frames or physical-monitor proof.
Commands, observations, PNGs and teardown/reap records are retained under
`.agent/scratch/native-home-proof-20261006/`; `density-final-evidence.json` supersedes the earlier
geometry-only density observations.

Remaining limits:

- **Home coverage is not whole-desktop screen-reader acceptance.** Other custom shell windows
  do not yet attach Home's adapter. `opt-a11y.seed` supplies Orca, at-spi2-core and speech-dispatcher;
  observed traversal does not imply an available speech server or braille device. Nested Wayland
  IME protocol coverage and physical fullscreen/output acceptance are not inferred from the
  successful owned X11 XIM journey.
- **Font coverage is limited.** The current DejaVu fonts do not cover the exercised CJK/emoji
  strings; missing-glyph boxes do not change their stored UTF-8, but are not acceptable glyph proof.
- **No high-contrast variant.** No high-contrast token, theme or setting exists in the shell.

One input method (`input-method` role).
