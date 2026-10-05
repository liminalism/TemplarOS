# TemplarOS integration contract

## Scope and present status

The target is your existing Ubuntu-based, Smithay-oriented TemplarOS compositor,
not a migration to another compositor. The application is a client of the
session's display authority. The connected `liminalism/TemplarOS` repository
returned an empty repository during this task, so this document is an integration
contract, **not an applied patch to current TemplarOS source**.

The delivered Wayland adapter runs `wlr-randr`, which requires the compositor to
advertise `zwlr_output_manager_v1` from wlr-output-management. The first gate is
simply to run `wlr-randr` in a real TemplarOS session. If it reports that the
protocol is unsupported, the GUI must not attempt xrandr against XWayland.

## Phase 1: basic display configuration

Implement output-management in the compositor alongside its existing output,
DRM and session structures. Follow the protocol version supported by the pinned
Smithay/protocol dependencies rather than copying a modern API signature into an
older checkout. No unverified `OutputManagerState::new`-style constructor is
assumed here.

Maintain an inventory of connected output heads, including disabled heads. A
mode/render surface being inactive must not remove the head from this inventory.
Advertise its connector name, product description, modes, preferred mode,
current mode, enabled state, position, transform and scale. Publish a coherent
inventory with a configuration serial. Finish/remove a head when it really
leaves the inventory, not merely when the user chooses Laptop only.

For every configuration request, validate the serial, mode handles, topology,
scale/transform and usable-output invariant. Build one candidate across all
outputs. Return cancellation on stale topology; do not quietly apply half of an
old configuration. Implement the protocol's test request without publishing a
new desktop layout or mutating the visible configuration.

Use the existing DRM backend to validate mode/resource feasibility. On an atomic
KMS path, test the applicable atomic state before committing it. Multi-GPU
changes are not magically one atomic transaction across devices: preserve a
rollback route when one device commits and another fails. Keep window/input
layout changes coherent with the actual outcome.

Apply screen position and logical scale to the compositor's output layout and
input mapping, not merely to its renderer's physical viewport. Re-home windows,
panels and focus whose previous output disappears. Confirm that disabling a
laptop output does not remove its physical connector from future recovery.

Once this protocol is functioning, the alpha supports ordinary Wayland
mode/refresh/position/rotation/scale operations without changing its UI or moving
display ownership into the application.

## Phase 2: compositor-owned safety

The long-term guarantee should remain true when the settings client and external
helper both die. Put the emergency connected-output fallback in the compositor's
hotplug/resume path. A future configuration preview needs a compositor-owned
lease with these semantics:

- A preview starts from an identified inventory generation and returns a unique
  preview token. Only one preview may own output configuration at a time.
- Its deadline uses a monotonic clock. Keep must arrive before that deadline
  and match the token and current topology. It cannot revive an expired preview.
- Client disconnect, expiry, apply failure, or topology loss restores a valid
  prior configuration or computes a fallback from currently connected outputs.
- A token, reason and committed generation are returned to the UI. User profile
  persistence happens only after confirmation.

These semantics are a proposed Templar extension or existing session-service
integration, **not claims about guarantees provided by the standard
wlr-output-management protocol**. The alpha uses an external helper and journal
until this stronger compositor ownership exists.

The compositor and the helper must not both independently restore profiles.
Choose an owner. In the final Templar architecture, the compositor enforces
safety, a session display service owns user intent/profiles, and the GUI presents
settings. Their configuration requests share a single serialized execution path.

## Phase 3: picture fitting as normal output composition

Expose a capability for safe-area fitting, with the allowed border ranges and
whether independent edges are supported. A private, versioned Templar control
interface can supplement standard output management for this feature; the
alpha does not implement that extra transport yet. Unsupported capabilities
must remain explicit in the UI.

For a physical signal W × H, define a safe rectangle
`(left, top, W - left - right, H - top - bottom)`. Keep this independent of the
logical output size and UI scale. Compose the entire logical output into that
rectangle and clear the surrounding signal pixels to black.

Use the compositor's established render and buffer-lifetime machinery. If a
hardware plane/scaler path is feasible, validate it through the normal KMS
resource checks. Otherwise the compositor may perform ordinary GPU composition;
this is not an xrandr panning workaround or a new custom video timing.

Account for the whole presentation path, not just wallpaper and application
surfaces. Cursor rendering/planes, drag icons, output rotation, fractional UI
scale, clipping, damage tracking, fullscreen surfaces and input coordinates must
agree about the safe rectangle. Bypass direct scanout when its path cannot honor
the same mapping; do not allow fullscreen video to bypass calibration.

For a simple unrotated output, inverse-map a physical pointer coordinate p to
logical coordinates using `logical_x = (p.x - left) * logical_width / safe_width`
and the corresponding y expression, with appropriate clipping. Integrate this
with the compositor's existing output transforms rather than stacking a second
unrelated coordinate system. Touch devices also need the correct inverse map.

Choose whether capture APIs return the logical desktop or the final physical
scanout, and keep that distinction explicit. A remote desktop need not inherit
black TV borders merely because a local TV needs calibration.

Persist a calibration per display/mode, with validation on resolution changes.
The full-screen calibration overlay should be compositor-placed on the intended
physical output, with a center control and keyboard nudges that remain reachable
when edges are initially cropped. Ordinary Wayland top-level positioning alone
is not a reliable output-selection contract for such an overlay.

## Shell/session integration

Register `display-settings` as the app's launcher role and route the monitor
settings entry to it. The included `X-TemplarOS-Provides` keys are declarations
for this integration, not proof that those role names already exist in your
registry. Run the display policy service once per actual graphical session and
pass the real environment. Do not hardcode display socket names.

For the final shell, a Win+P-like shortcut can present the four intent choices
and submit the same safe configuration transaction. It must not bypass validation
or write separate display profiles. The alpha leaves global shortcut routing to
the desktop; it does not install a key grabber.

Qualify external-screen removal, login with a TV connected, laptop lid behavior,
suspend/resume, hybrid-GPU output routing, and focus/window recovery before
replacing the existing display route in an image. Leave HDMI audio, brightness
and suspend policy to their existing services unless separately scoped.
