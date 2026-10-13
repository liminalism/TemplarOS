# Launcher, rail and Homegarden

The launcher discovers and opens applications, files and folders. The bottom rail returns to
running tasks and saved launch targets. Homegarden shows projects, sessions and their current
work. All three use the same application catalogue, verified window identity and brokered
launch service.

## Open and use the launcher

Press **Super+Space**, click the rail's launcher control, or choose **Open launcher** in Home's
context menu. The shell opens one focused overlay above the current application, including a
maximized or fullscreen application, without revealing Home or changing its window geometry.
Typing on bare Home also forwards the text to this popup.

The rail's launcher control wears a drawn magnifying glass. With the default gesture layout,
swipe left along the bottom row to reveal running windows and saved applications; swipe right
to reveal System. You can start anywhere in the bottom eight logical pixels, including with
the cursor already resting there. The existing approach down into the centre gate and then
left or right also works. After closing a rail, briefly pause or leave the bottom row before
starting another swipe. Opening either rail dismisses app search. Keep the pointer in the
bottom row while travelling to the centred controls; a swipe from the opposite side fully
reveals them. Click the magnifying glass to open search.

The popup starts in **Apps** with keyboard focus in the search field. With no query it shows
favorites, then recent applications, suppressing duplicate plain application targets. Typing
uses the existing ranked application matcher. **Tab** switches to **Files** and back while
keeping the query. The popup keeps a stable size with a scrollable result list.

| Input | Result |
|---|---|
| Click or Enter | Activate a running application if available, otherwise launch it; open a file or folder immediately. |
| Ctrl+Enter on an application or saved profile | Request a new instance. |
| Up/Down or Page Up/Page Down | Navigate results and scroll them into view. |
| Right on an application or saved profile | Show its existing windows through the shared task/window path. |
| Escape or click outside | Close the popup without opening a result. |
| Right-click | Open actions for that result. |
| Delete after keyboard list navigation | Hide that result from search. |

Opening a result never creates a Home shortcut. Home's existing tile and window-card drags
remain available for organizing objects that are already there.

## Files mode

A blank Files tab shows recent files and folders known to the shell. A nonempty query searches
all indexed roots through **Files.Index1**, retaining native Unix path bytes. Rows show the path
and available modification/size metadata. Results arrive in bounded pages; close, reopen,
query and mode changes invalidate old responses and close superseded cursors.

Files open through **Associations1** and **Launcher1**. Folders open in the selected Files
provider. The file manager's **Search here…** action opens this same popup with an explicit
directory scope. The equivalent CLI request is:

```sh
templar files search-here ~/Projects --query fire
```

The default global launcher remains unscoped. FSearch provider routing and fork packaging
are retired; Files.Index1 and its other clients remain. Earlier prototype plans and benchmark
reports describe the historical FSearch experiment.

## Save, rename and hide

Right-click a result for explicit actions:

| Action | Effect |
|---|---|
| Pin to applications rail | Save an application launch target; this action is offered for apps and saved profiles. |
| Add shortcut to Home | Create a Home shortcut. A saved terminal profile copies its full launch configuration. |
| Rename shortcut | Change launcher metadata or a saved profile's label. |
| Get info | Show application identity/provenance or file path/type and available metadata. |
| Hide from search | Filter that application or native path from launcher results. |

Renaming a shortcut does not modify an installed desktop entry or rename a file. Hiding a
result changes launcher preferences; it does not uninstall an application, delete a file or
hide a Homegarden Place. Restore results with **Unhide** under **Settings → Launcher & Files →
Launcher**.

That page also creates, edits, reorders and removes favorites. A terminal profile stores a
name, application, working directory and literal arguments. Enter an absolute directory or
a lossless `file:` URI; add each argument separately. Launching uses
`Launcher1.LaunchInDirectoryV1`, so directory names and arguments are never interpreted as
shell commands. A matching verified profile instance is activated when available;
Ctrl+Enter explicitly starts another.

The rail reconciles saved plain applications with live task units, so a saved app that is
running occupies one chip. Running tasks retain authoritative `TaskKey` grouping and stable
first-seen order. Named terminal profiles remain distinct launch configurations. A copied
Home profile remains usable even if its rail favorite is later changed or removed.

## Settings and motion

Settings groups destinations into Network & Bluetooth, Sound, Displays, Input, Power,
Appearance & Motion, Desktop & Homegarden, Launcher & Files, Applications & Defaults, and
Advanced & Diagnostics. Algorithm controls, experimental Home throw, rail geometry and lane
layout are in Advanced. Preview update rate appears only for live previews. Obsolete
FSearch/Home-search controls and unused ordinary grouping/enforcement choices are removed
from normal navigation.

Under **Appearance & Motion**, choose the window transition: **essence** (particles),
**simple** (motion), or **off** (no animation). Essence grain size defaults to
**6 logical px**, with a **1–24** range. Size changes update drawing uniforms without
recomputing movement paths. The existing
`shell.home-field.reduced-motion` preference overrides these animations; enabling it or
choosing Off cancels active visual motion while retaining the committed window state.

Essence is drawn in the compositor's GLES scene using bounded GPU snapshots and the window's
Home target. Rapid park/restore reverses one movement plan. Unsupported shader or capture
capabilities fall back to Simple. Material and glass/bar shader experiments are separate
sandbox features. Headless interaction fixtures, private compositor sessions and GPU tests
exercise specific paths; installed-session feel and integrated-GPU performance still require
observation on the target machine.
