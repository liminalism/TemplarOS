# External window tabs — advisor's review, 2026-10-01 (sections 3 and 4)

Received by the owner from an outside advisor and pasted into the agent session on 2026-10-02.
The advisor inspected the September 17 source archive, not the live tree. Verbatim except for
this header and curly quotes normalised to straight ones; sections 1-2 (display freezes,
Omarchy) are not included.

## 3. External window tabs: retain stable locations, reveal them when needed

Your core principle is coherent:

> **Application pixels belong to the application. Window-management controls belong outside it and appear only when useful.**

The part I would resist is making control placement too dependent on the history of mouse movements.

### Resize-driven relocation: useful as an optional hint, weak as the default

The attraction is clear: pulling a window upward suggests that controls should be accessible above it; pulling downward suggests the bottom.

The problem is that **resize intent and control-placement intent are not necessarily the same**. Someone may shrink a window from its top edge simply because that edge is accessible—not because they want future controls moved.

It also creates a memory burden: the same window's minimize button might be above or below depending on an earlier resize.

I would keep this option, but constrain it:

**Use the latched resize edge to suggest the next tab location, apply it after the grab ends, and never move a visible or pressed control during that interaction.**

Do not infer it continuously from instantaneous cursor direction. Resizing contains corrections and reversals; those should not make the controls oscillate between edges.

An explicit ability to **pin a window's tabs to the top or bottom** would be more understandable than silent permanent adaptation.

### Exterior hover reveal: the stronger default

This better addresses the specific problem you reported—your hand expects controls at the top.

I would implement **fixed top and bottom anchors**, not controls that literally follow the pointer:

| Behavior | Proposed rule |
|---|---|
| Pointer remains inside the application | No controls appear. |
| Pointer enters the eligible external top-control region | The control cluster appears at its fixed top anchor. |
| Pointer moves between revealed controls | Their positions stay fixed. |
| Pointer leaves briefly | A small exit grace period prevents flicker. |
| Pointer presses a control or begins a drag | Placement and lifetime remain latched until completion. |
| Window is covered by another window | Its hidden controls must not steal the higher window's input. |

For your own default, **put the control cluster at the top**, because that is where you already look for it. That does not require a conventional titlebar: three small external tabs can remain completely absent until called.

The name tag can stay horizontal and separate. There is no need to rotate its text simply because controls sometimes occupy a side. Personally, I would exhaust top/bottom placement before introducing a vertical side-mounted control arrangement.

A bottom default with a **temporary, anchored top copy of the same actions** is also workable. But do not make the lower controls visibly jump upward as the pointer approaches. Reveal a stable alternate affordance and keep both connected to the same underlying actions.

Most importantly: **revealing a Close button must not execute Close**. The button needs to be visibly presented and receive an intentional click.

### Keep resize zones and control zones separate

This is where your current bottom-corner problem becomes relevant.

The geometry code **already implements bottom resizing**:

```text
crates/templar-window/src/geometry.rs:524–579
```

The suspicious part is the relationship between the visible application and the abstract frame.

Currently:

- The abstract frame includes a **32-pixel bottom tag strip**.
- The client area is the frame minus that strip.
- Resize hit-testing accepts points **outside the abstract frame**.
- Inside-frame presses are tested against tag controls first.

Therefore, the visible application's bottom corner and the frame's resize corner are not necessarily the same place. A press just below the application can land on a control tag or the transparent gap between tags. That gap explicitly passes through to whatever is behind.

This is a **credible explanation to check**, not a confirmed reconstruction of your particular click. The relevant routing is:

```text
compositor/templar-comp/src/input.rs:276–308
```

There is a second possibility: an outside-only resize zone becomes unreachable when a window ends up flush with a screen edge. Your geometry code already acknowledges that problem and adds margins during initial placement, but every resize, maximize/restore, and output-change path needs consistent treatment.

The fix should be tested at the **visible corners through the actual input router**, not only through unit tests of `resize_edges_at`.

I would reserve a small exterior corner region exclusively for resizing and inset the control tabs from it. Close and resize should never compete for the same press location.

### This needs a geometry refactor, not just a drawing change

At present, "subtract the bottom tag strip" is built into the client/frame relationship. That assumption becomes wrong once tabs can be above, below, hidden, or split across edges.

Have `WindowService` produce one authoritative layout containing:

```text
client rectangle
external name/control rectangles
painted regions
interactive regions
resize regions
recovery target
```

Rendering, hit-testing, placement, recovery, and damage tracking should all consume that same result.

Then make this an invariant:

> **Showing, hiding, or relocating an external tab does not resize or reconfigure the application.**

Otherwise a cosmetic hover can cause terminal reflow, browser relayout, redraws, and coordinate changes—exactly the wrong coupling for both usability and stability.

There is also an unavoidable fullscreen constraint: **external controls cannot occupy visible space outside an application that already occupies every visible pixel**. Choose explicitly between a small exterior gutter for ordinary/maximized windows and a Taskfield/keyboard/global recovery route for true fullscreen. Do not silently overlay the application while claiming the controls remain external.

## 4. The direction I would pursue next

You have enough interaction ideas now to form a consistent system. The next step is to make them obey a few reliable rules:

**Hidden controls should be discoverable, not unpredictable.** A temporary "show available controls" modifier could reveal tabs and resize targets without leaving permanent decoration.

**Control vocabulary should remain familiar.** Stable minimize/maximize/close symbols and action order matter more than giving them novel names. Where "Park" genuinely differs from minimize, explain that distinction rather than presenting the two as interchangeable.

**Optional presentation work should never determine desktop liveness.** Previews, animated reveals, icon loading, and other decoration must be expendable before input and application presentation. For the gesture rail's icon polish, use cached, consistently sized icon assets rather than adding decoding or theme lookup to its pointer path.

My recommended order is:

1. **Instrument and correct the display path**, beginning with synchronization handling, completion/error state, bounded capture work, and consolidated repaint scheduling.
2. **Fix bottom-corner hit geometry independently** of any tab redesign.
3. **Prototype anchored exterior hover controls**, with no client resizing and no mid-interaction relocation.
4. **Borrow Omarchy's installer tests and selected presentation components**, then decide whether its backend port or Calamares better serves your installation goal.
5. **Test an alternative kernel against a stable baseline**, rather than using it as an unmeasured cure for the freezes.
