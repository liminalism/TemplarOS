**Yes—this is worth prototyping, and it could be substantially better than adding more edge-triggered popups.** I would build it as **gesture-revealed menu rails**: the gesture chooses a direction, then the cursor uncovers stationary controls along that direction.

The most important distinction is this:

> **The cursor draws the menu into view; it does not drag the menu’s targets along with it.**

Keep your two-line app tray as the content of the Apps branch. Replace its invocation and dismissal behavior, and give the system controls the same interaction model. There is no need to replace application identity, window switching, or the underlying system services.

The gesture recognition itself should be inexpensive. The harder problems are avoiding unintended activation, keeping revealed controls reachable, and making rendering follow the pointer without queuing work or dropping the final update.

## 1. Refine the gesture into a precise interaction

I am treating “drag down-left/right” as **moving the mouse without holding a button**. An actual button-held drag should normally suppress invocation, so selecting text or moving a window through the region does not open a menu.

### The invisible box should arm a gesture, not open anything

I would start with an invisible rectangle approximately **180 × 70 logical pixels**, centered horizontally and elevated enough to leave roughly **120 pixels below its lower edge**. These are prototype values, not established ergonomic optimums.

The elevation matters. Putting the trigger directly against the bottom edge leaves almost no room to distinguish downward motion or accommodate both tray rows.

My proposed first-version contract is:

| Mouse behavior                             | Result                                                   |
| ------------------------------------------ | -------------------------------------------------------- |
| Move horizontally through the box          | Nothing.                                                 |
| Move upward through it                     | Nothing.                                                 |
| Move straight downward through it          | Nothing yet.                                             |
| Enter from above, then continue down-left  | Reveal System.                                           |
| Enter from above, then continue down-right | Reveal Apps.                                             |
| Hold a button while entering               | Do not begin a gesture.                                  |
| Reverse direction after a branch opens     | Navigate back along that branch; do not switch branches. |

**Requiring an approach from above is my main refinement to your idea.** It makes the box a directional entrance rather than a new hot corner in the middle of the desktop. It still permits one continuous diagonal sweep; it does not require a separate downward stroke followed by a turn.

The direction mapping can be reversed with one setting.

There is an unavoidable limitation: **an ordinary mouse movement that exactly matches the intended gesture is indistinguishable from an intentional invocation.** Recognition can reduce false positives, not eliminate them. That makes a configurable gate, an off switch, and ordinary-use testing more important than a sophisticated classifier.

### Use the diagonal to choose the branch, then unfold sideways

Your down-left and down-right directions make sense as two distinct commands. I would not require a long diagonal journey through the entire menu, though: a lengthy diagonal rail would quickly run off the bottom of the screen.

Instead, use a short diagonal lead-in followed by a mostly horizontal rail:

* The initial down-left movement commits to System, which unfolds leftward.
* The initial down-right movement commits to Apps, which unfolds rightward.

At commitment, **freeze the rail’s position and item layout**. Subsequent movement changes how much of it is visible, not where its controls are.

For the two-line app tray, reveal **both rows together, column by column**. Align one row with the outgoing pointer path and place the second above it, rather than directing the cursor into the gap between rows.

This preserves the feeling of drawing something out without creating moving targets.

### Reveal slightly ahead of the pointer

A completely literal “only show pixels the cursor has already crossed” rule is awkward: the user cannot see a target until they are already on it.

I would reveal approximately **half an item ahead**. Thus the cursor leads the unfolding motion, but the next target becomes legible before selection.

Once a section has appeared, keep it visible until dismissal. Do not retract the rail immediately when the mouse moves back toward an earlier item.

That gives you three useful behaviors:

**Sweep outward to discover more. Move backward to revisit. Click to act.**

A stationary hover may reveal a label, preview, or subordinate panel. It should not launch an application, change a setting, or initiate a session action merely because the stroke ended there.

### Keep the directional vocabulary small

This resembles *marking menus*: a stable direction can become a learned route to a command, while visible menus support discovery. The classic research is relevant to keeping the gesture vocabulary simple, but it does **not** establish that an invisible, button-free trigger will have an acceptable accidental-activation rate. That part needs your own testing. 

I would defer the third, upward branch until left/right works well. With an entrance from above, an upward movement could mean either “cancel/back out” or “choose the third branch.” That ambiguity needs an explicit rule rather than another angle added to the classifier.

## 2. What I would reuse and replace in your code

I inspected the actual archive again for this proposal. The relevant implementation is already divided into pieces that can support the change.

One snapshot qualification: the archived `BarScreen` still lays out a single bar row; I did not find your described two-row layout there. **Your current two-line tray should be preserved**, not replaced with the older layout from this archive.

| Existing component                                  | What the snapshot does                                                                   | Recommended change                                                                                              |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `compositor/templar-comp/src/input.rs`              | Handles relative and absolute pointer movement; also owns edge-dwell recognition.        | Add the directional recognizer here, covering both input paths.                                                 |
| `shell/templar-shell/src/surface/bar_screen.rs`     | Has its own hint, reveal, and dismissal state.                                           | Retain useful tray rendering/actions, but replace its activation policy in gesture mode.                        |
| `shell/templar-shell/src/surface/cluster_screen.rs` | Implements system controls, popovers, and slider behavior.                               | Reuse those controls behind the System rail.                                                                    |
| `shell/templar-shell/src/surface/app.rs`            | Creates/destroys the system-cluster surface as coverage changes.                         | Avoid recreating the presentation resources on every reveal; use a bounded persistent host or cached resources. |
| `shell/templar-shell/src/wayland_bridge.rs`         | Already supports masked ARGB presentation and explicit input regions.                    | Build on this rather than introducing a new rendering toolkit.                                                  |
| `interfaces/wayland/templar-shell-v1.xml`           | `bar_peeked` communicates an output being peeked, but not gesture direction or progress. | Add a versioned gesture-session extension instead of overloading the existing event.                            |

The exact excerpts and original line numbers are in the [snapshot source notes](sandbox:/mnt/data/templaros_gesture_rails/SOURCE_NOTES.md).

**Do not keep the old and new rail activators running simultaneously.** In gesture mode, retire the old `BarPeek`/`BarPromote` behavior for these rails and the shell’s hint dwell. Otherwise you will have several mechanisms opening and closing the same interface.

Keep the compositor’s independent window-recovery routes until you deliberately replace them. Scrapping the current popup design should not accidentally remove your way out of fullscreen or an inaccessible window.

Also override the current rule that opens the bar automatically when Home is uncovered. Your proposed mode says “hidden until called,” including on an otherwise empty desktop.

## 3. Put recognition in the compositor—not in a transparent window

The ownership split should be:

**Compositor:** recognizes the gesture, owns the active invocation, applies safety rules, and handles stacking/input routing.

**Shell:** lays out and paints the rail, supplies application/system content, and runs the actual menu controls.

The invisible trigger box should be **a rectangle in compositor state**, not a transparent shell window.

A transparent window is still an input surface unless its input region says otherwise. Wayland distinguishes visual transparency from input acceptance; an explicitly empty input region passes events through. Importantly, `set_input_region(None)` means unrestricted input, **not** an empty region. ([Wayland][1])

Your bridge already has the appropriate helper:

```rust
// Hidden host: no part of it accepts pointer input.
// This does not itself hide painted pixels; clear/unmap those separately.
bridge.declare_input_shapes(&window, &[])?;
```

While the gesture is merely being considered, the underlying application continues receiving normal pointer events. Recognition observes the stream; it does not take a pointer grab or suppress normal delivery.

Smithay’s grab API explicitly changes pointer behavior and cautions that failing to forward events changes what the rest of the compositor sees. You do not need that machinery just to recognize an approach gesture. ([Docs.rs][2])

### Use one small state machine

The basic lifecycle should be:

```text
Hidden → Armed → Open → AwaitExit → Hidden
```

**Hidden:** no gesture-specific redraws, timers, IPC traffic, or input interception.

**Armed:** a recent eligible entrance has occurred; small movements are ignored until direction is sufficiently clear. A timeout invalidates stale intent.

**Open:** one branch is latched. The cursor reveals and interacts with that branch.

**AwaitExit:** after dismissal, the pointer must leave a slightly enlarged trigger region before another approach can arm it.

That final state prevents annoying open–close–open loops while the pointer remains near the trigger.

### Do not close the menu during legitimate interaction

Once open, the rail and its child popover should share one lifetime. A small gap between an icon and its popup must not be treated as “the user left.”

I would use a **180–250 ms departure grace period**, with the deadline cancelled when the pointer reaches the rail or its popup. A geometric corridor toward the child panel can extend that tolerance.

That corridor is for **keeping the menu alive**, not for invisibly catching clicks on the application underneath.

There is also an important grab distinction:

* A button held **before invocation** prevents the gesture.
* A button held while dragging a slider **inside the open menu** must not close it.
* A foreign drag, window resize, or other conflicting interaction cancels/suppresses it.

A blanket `pointer.is_grabbed()` check throughout the open state would get that wrong.

Active pointer locks and confinement need separate handling. The pointer-constraints protocol can lock the cursor while relative-motion events continue; those relative deltas must not be interpreted as movement through your desktop gate. ([Wayland][3])

## 4. How to make it performant

**The essential rule is: recognize every relevant input sample, but render only the newest visual state.**

These are different workloads and should have different cadences.

### Recognition: a few scalar operations per event

The recognizer only needs:

* The previous logical pointer position.
* The gate geometry and output identity.
* An entrance point and timestamp.
* A current phase and selected direction.

It does not need a stored gesture path, machine learning, a daemon, an animation thread, or repeated service calls.

Use **logical screen coordinates after the existing input transformation**, not raw device counts. Apply the same recognition logic to the relative and absolute pointer paths.

Crucially, test **the segment between consecutive pointer samples**, not just whether the latest point lies inside the box. A fast movement can cross the entire box between events.

The reference code includes that case.

There is a small existing optimization opportunity: `on_pointer_motion` currently collects output rectangles into a `Vec` on each report. Cache that geometry when outputs/layout change. This is separate from the gesture recognizer; adding an allocation-free recognizer does not make the whole existing input path allocation-free. See `input.rs:1060–1065` in the source notes.

### Rendering: fixed geometry, changing clip

At invocation:

1. Choose and freeze the rail anchor.
2. Lay out its items at their final positions.
3. Establish the bounded surface size.
4. Begin revealing it by changing a clip/mask.

Do **not** resize the native window for every few pixels of pointer movement. Do not recenter the whole tray as more items appear. Do not remeasure every label on each event.

The clip should determine what is visible, while actual item rectangles remain stable. Input regions should be updated consistently with the successfully presented content so invisible controls do not become clickable ahead of the drawing.

Wayland frame callbacks are designed to pace redraws and animation; they are not a reason to redraw once for every mouse report. ([Wayland][4])

Send the initial **Begin** event immediately. Do not wait for a callback from a hidden/unmapped surface to start showing it. After that, coalesce visual progress to the selected output’s presentation cadence.

### Fix the busy-buffer case before polishing the animation

There is a concrete issue in the current presentation API:

`present_masked()` checks whether its buffer has been released. When the buffer is busy, it skips the presentation and returns `Ok(())`.

Respecting buffer ownership is correct. The problem for this feature is that **“successfully submitted” and “deferred because busy” are indistinguishable to the caller**.

Imagine this sequence:

1. You sweep to Network.
2. The last reveal/highlight update arrives while the buffer is busy.
3. That update is skipped.
4. You stop moving.
5. No subsequent motion supplies another update.

The display must still catch up.

I would change the presentation result to distinguish:

```rust
enum PresentOutcome {
    Submitted,
    Deferred,
}
```

Then retain the latest dirty state until it is actually submitted. Buffer release and frame readiness must wake the pending work appropriately. They are different signals: one permits buffer reuse, the other paces drawing. Wayland’s buffer-release and surface-frame mechanisms have distinct roles. ([Wayland][1])

A small reusable two- or three-buffer pool is a sensible implementation, but **a pool does not replace correct pending-state handling**.

The snapshot locations are `wayland_bridge.rs:1619–1678` and the release handler in `previews.rs:582–598`.

### Avoid accidentally redrawing the whole desktop

Your shell currently responds to changes in its shared bridge generation by requesting redraws for Home, the bar, and Taskfield.

**Gesture progress should have a separate dirty path.** Moving another ten pixels along System should not look like a global toplevel-model change and redraw three unrelated surfaces.

Similarly, invocation must not synchronously enumerate applications, resolve icons, query Wi-Fi, fetch device status, or capture window previews. Use the existing cached/asynchronous models.

The system slider implementation already separates immediate visual feedback from throttled service updates. Preserve that useful behavior.

### Performance targets—not claims about the current build

I would measure these separately:

| Measurement                          | Initial acceptance target                                                       |
| ------------------------------------ | ------------------------------------------------------------------------------- |
| Hidden, stationary desktop           | No additional periodic wakeups attributable to this feature.                    |
| Recognizer cost                      | Constant-time, allocation-free; investigate a p99 above approximately 10 μs.    |
| Recognition-to-visible-feedback      | Aim for 1–2 refresh intervals after commitment, then measure natively.          |
| Pointer stops while buffers are busy | Final reveal and highlight still appear without another motion event.           |
| Input flood                          | No growing queue of obsolete visual progress.                                   |
| Menu invocation                      | No filesystem, process-launch, or synchronous service work in the pointer path. |

Even a seemingly small per-event cost matters at high report rates: a hypothetical 10 μs at 8,000 events per second is 80 ms of CPU time per second—8% of one core. That is arithmetic, not a measurement of this implementation.

Do not smooth or warp the actual cursor to make the reveal look attractive. Decorative easing can be added later, but the cursor and hit targets should remain direct.

## 5. Code samples

I prepared a dependency-free Rust reference with the recognizer, reveal geometry, pending-frame helper, and 30 unit tests:

**[Download the Rust reference and integration notes](sandbox:/mnt/data/templaros_gesture_rails.zip)**

These are **reference files, not an applied TemplarOS patch**. A Rust compiler was unavailable here, so the included Rust tests remain unrun. No native performance result is claimed.

### A. Detect an entrance even when the mouse skips over the box

This is the important geometric primitive. `Point` and `Rect` are defined in the reference package.

```rust
/// Coordinates are logical pixels; y increases downward.
/// Returns the point where a movement segment entered the gate from above.
fn top_entry(r: Rect, a: Point, b: Point) -> Option<Point> {
    if a.y >= r.y || b.y < r.y || b.y <= a.y {
        return None;
    }

    let t = (r.y - a.y) / (b.y - a.y);
    let x = a.x + t * (b.x - a.x);

    if x < r.x || x > r.x + r.w {
        return None;
    }

    Some(Point { x, y: r.y })
}
```

After entry, classify displacement using a broad diagonal cone rather than a precise angle or minimum velocity:

```rust
fn branch(entry: Point, current: Point) -> Option<Side> {
    let dx = current.x - entry.x;
    let dy = current.y - entry.y;
    let horizontal = dx.abs();

    // Prototype thresholds, in logical pixels.
    if horizontal < 30.0 || dy < 18.0 {
        return None;
    }

    // Reject almost-horizontal and almost-vertical movements.
    // No atan2 is needed.
    if dy < 0.35 * horizontal || dy > 2.8 * horizontal {
        return None;
    }

    Some(if dx < 0.0 {
        Side::Left
    } else {
        Side::Right
    })
}
```

Those functions are only the geometry. The supplied `Recognizer` adds timeout, rearming, output/geometry changes, cancellation, and pre-invocation button inhibition.

It intentionally does not execute actions or take ownership of pointer input.

### B. Make the rail grow without moving or retracting its targets

The core reveal calculation is:

```rust
pub fn advance(&mut self, pointer: Point) -> bool {
    if !pointer.x.is_finite() || !pointer.y.is_finite() {
        return false;
    }

    let direction = match self.side {
        Side::Left => -1.0,
        Side::Right => 1.0,
    };

    let requested = (
        direction * (pointer.x - self.anchor.x) + self.lead
    ).clamp(0.0, self.length);

    // Reversing toward an earlier control must not hide it.
    let next = self.visible.max(requested);
    let changed = next > self.visible;

    self.visible = next;
    changed
}
```

Here, `anchor`, `length`, and item positions were fixed at invocation. `lead` provides the small visible area ahead of the cursor. Only `visible` changes.

For the two-line tray, its clip covers both rows; the row layout is independent of this calculation.

### C. Keep the latest visual state until presentation succeeds

The package’s `LatestFrame<T>` helper retains a newest pending state and tracks which revision actually reached submission.

The intended integration pattern is:

```rust
// Every visual change replaces the pending value.
// It does not enqueue another frame behind older values.
pending.publish(new_visual_state);

// When the presentation path is ready:
if let Some((revision, state)) = pending.pending() {
    match presenter.try_present(state)? {
        PresentOutcome::Submitted => {
            pending.did_submit(revision);
        }
        PresentOutcome::Deferred => {
            // Keep the pending revision.
            // Retry when buffer/frame readiness changes,
            // even if the mouse sends no further motion.
        }
    }
}
```

`try_present` and `PresentOutcome` here describe a **proposed adapter contract**, not existing methods in your snapshot. The reference helper ensures an older submission cannot erase a newer pending update.

The [full integration scope](sandbox:/mnt/data/templaros_gesture_rails/README.md) identifies the compositor, protocol, shell, and lifetime changes needed around these primitives.

## 6. Scope the first implementation narrowly

I would divide this into four acceptance milestones.

**1. Recognizer and diagnostic gate.**
Draw the box only in a debug/learning mode. Record which movements arm, commit, or cancel. Test ordinary crossings, text selection, window dragging, fast sweeps, and slow deliberate gestures before any real menu takes input.

**2. One neutral rail.**
Use numbered dummy cells to prove stationary targets, progressive revelation, backtracking, dismissal, and flick-and-stop behavior. This isolates interaction and presentation from application/system plumbing.

**3. Attach the existing Apps tray and System controls.**
Preserve the two-line layout and existing actions. Give the rail and its child panels one coherent lifetime. Disable the competing legacy rail triggers only when this mode is selected.

**4. Add the small configuration surface and use it normally.**
Expose mode, swapped directions, trigger size/inset, a learning hint, and application exceptions. Keep keyboard access. Promote it to your personal default after you have observed both intended successes and accidental openings during ordinary work.

For the first version, I would specifically leave out arbitrary gesture editing, multiple gesture hierarchies, the third branch, elaborate effects, and changes to menus owned by other applications. This should initially govern **TemplarOS’s own shell surfaces**, not attempt to redefine every GTK/Qt application’s menu system.

**My recommendation is to proceed—but make the innovation the predictable interaction, not the animation.** A short directional entrance, a stationary menu revealed by the pointer, stable item order, and forgiving popup transitions would give TemplarOS something coherent. An invisible hot area that opens a moving collection of targets would reproduce the frustration you are trying to remove.

[1]: https://wayland.freedesktop.org/docs/html/apa.html?utm_source=chatgpt.com "Appendix A. Wayland Protocol Specification"
[2]: https://docs.rs/smithay/latest/smithay/input/pointer/trait.PointerGrab.html?utm_source=chatgpt.com "PointerGrab in smithay::input::pointer - Rust"
[3]: https://wayland.app/protocols/pointer-constraints-unstable-v1?utm_source=chatgpt.com "Pointer constraints protocol | Wayland Explorer"
[4]: https://wayland.freedesktop.org/docs/html/apa.html "Appendix A. Wayland Protocol Specification"
