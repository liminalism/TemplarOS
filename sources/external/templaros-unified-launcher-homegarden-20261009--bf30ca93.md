# TemplarOS: unifying the launcher, gesture bars, and Homegarden

## 1. Architectural recommendation

After examining the October 9 snapshot, I would make one significant change to TemplarOS's interface philosophy:

The Homegarden should represent your working environment. The bottom gesture system should navigate that environment. The new launcher should be the single place for discovering and opening applications and files.

These should share the same application identity, window state, favorites, and action dispatch rather than maintain separate versions of those concepts.

The underlying architecture already supports much of this. You have:

- `templar-shell`, built around PixelKit, with an existing application catalogue, launcher, search presentation, and gesture rails.
- `templar-sessiond`, which owns application identity, launching, settings, and history.
- `templar-comp`, which owns the actual windows and gesture recognition.
- `templar-filed` and `Files.Index1`, providing the file-search service.
- `HomeGarden1`, providing the working-place and session representation.

The opportunity is to remove competing presentations, not replace working infrastructure.

I would keep the two gesture directions, the existing window/task identity model, and the Homegarden's spatial representation. I would remove the permanently displayed Home search field and its separate recent-app strip, replace the FSearch dependency with one PixelKit launcher popup, and make both the bottom bar and Homegarden invoke that same popup.

One important interaction rule: a normal click activates something; organizing or creating a shortcut requires an explicit action. The launcher should never quietly turn a selected result into a Homegarden tile.

## 2. What I would change in the existing implementation

The principal files are:

| File                                                | Change                                                                                                      |
| --------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `shell/templar-shell/src/surface/field_screen.rs`   | Remove the permanent Home search field, its recent-app row, and search-result drag-to-Home behavior.        |
| `shell/templar-shell/src/surface/omnibox_screen.rs` | Reuse its result-row rendering and keyboard navigation in a standalone launcher surface.                    |
| `shell/templar-shell/src/omnibox.rs`                | Retire the FSearch-specific row and the assumption that Files search must be contextual to the open folder. |
| `shell/templar-shell/src/surface/app.rs`            | Own popup creation, focus, dismissal, search dispatch, and activation.                                      |
| `shell/templar-shell/src/surface/bar_screen.rs`     | Add persistent favorites alongside running tasks, without creating duplicate icons.                         |
| `shell/templar-shell/src/taskbar.rs`                | Keep it as the authoritative projection of running tasks, not stored shortcuts.                             |
| `crates/templar-session-core/src/settings.rs`       | Add launcher preferences, hidden results, favorites, and animation configuration.                           |
| `apps/templar-settings/src/surface.rs`              | Expose useful configuration and prune obsolete controls.                                                    |

There are three important discoveries in the code.

First, single-click launching is mostly implemented already. `FieldScreen::choose()` activates an existing application window, launches one if none is running, opens a file, or opens a directory in Files. `OmniboxScreen` also has click-to-commit behavior. However, `press_on_result()` intercepts the press and prepares a draggable Home tile before deciding at release whether the gesture was a click. This is unnecessary complexity for the new popup. I would remove that coupling rather than try to repair it.&#x20;

Second, the existing Files search can already query Index1. `templar_client::file_index::SearchSession` manages searches, cursors, freshness, and cancellation; `app.rs::run_search()` already uses it. You do not need the FSearch fork, and you do not need to rewrite the search service to produce a new launcher.&#x20;



Third, the launcher must become a real shell overlay. The current search belongs to the Home surface, while the existing bottom bar and system cluster use `Layer::ShellOverlay`. A global popup painted only into Home could disappear behind application windows. The right implementation is another independently focused PixelKit surface, managed by the existing shell process.&#x20;

## 3. The unified interface

Launcher — Applications

Super+Space

Search applications…

Apps

Tab for Files

Esc closes

FAVORITES

Development terminal

Opens in \~/Projects

Files

File manager

RECENT APPLICATIONS

Firefox

Running · activate existing window

Enter: open · Ctrl+Enter: new instance

This is an interaction mockup, not a screenshot of the running shell. The actual popup should use your existing PixelKit typography, spacing, icons, and visual treatment.

On opening, the text field has keyboard focus and Apps is selected. No query is required to display useful results: show favorites first, then recent applications, with duplicates suppressed.

Once the user types, replace those sections with ranked application matches. Pressing Tab switches to Files and searches the same typed text. Another Tab returns to Apps. Keep the typed text when switching so `fire` can immediately change from Firefox to filenames containing `fire`.

The popup should be roughly 700 logical pixels wide on a desktop, with a fixed maximum height and a scrollable result list. Do not resize it every time results arrive; that causes rows to shift under the pointer.

A click or Enter should act immediately and close the popup. Escape dismisses without acting. Right-click opens a result-specific context menu. Clicking another window, Homegarden, or empty desktop should dismiss the popup without activating a search result.

The existing `Ctrl+Enter` behavior is useful: normal activation prefers a running instance; Ctrl+Enter explicitly launches another one. I would retain that distinction.

## 4. Model the launcher as one component with two modes

I would add `surface/launcher_screen.rs` for rendering and `launcher_model.rs` for state, keeping the same separation between pure UI and privileged actions that the Home Field already uses.

A simplified state model:

```
use std::path::PathBuf;
use templar_types::AppId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LauncherMode {
    #[default]
    Apps,
    Files,
}

#[derive(Clone, Debug)]
pub enum LauncherTarget {
    App(AppId),
    File(PathBuf),
    Directory(PathBuf),
    Favorite(String),
}

#[derive(Default)]
pub struct LauncherState {
    pub open: bool,
    pub mode: LauncherMode,
    pub query: String,
    pub highlighted: usize,
    pub generation: u64,
}

impl LauncherState {
    pub fn show(&mut self) {
        self.open = true;
        self.mode = LauncherMode::Apps;
        self.query.clear();
        self.highlighted = 0;
        self.generation += 1;
    }

    pub fn switch_mode(&mut self) {
        self.mode = match self.mode {
            LauncherMode::Apps => LauncherMode::Files,
            LauncherMode::Files => LauncherMode::Apps,
        };
        self.highlighted = 0;
        self.generation += 1;
    }

    pub fn dismiss(&mut self) {
        self.open = false;
        self.generation += 1;
    }
}
```

The `generation` is important because the user can close the popup, reopen it, or switch modes while an Index1 query is still pending. An old response must not repaint the new view.

In `LauncherScreen::draw()`, process Tab before passing keyboard input to `OmniboxState::handle_keys()`. Currently, `omnibox_screen.rs` interprets Tab as `Intent::ShowAll`, opening the task overview. That behavior must change for the launcher specifically; the Right arrow can retain the existing show-windows action.

For result rows, PixelKit already exposes precisely the pointer primitives you need:

```
if ui.input.take_context_click(rect) {
    state.highlighted = index;
    actions.push(LauncherAction::ShowContextMenu(index));
} else if ui.input.take_click(rect) {
    state.highlighted = index;
    actions.push(LauncherAction::Open(index));
}
```

This should be the entire normal-click contract. No drag preparation, no delayed second click, and no separate selection-then-activation state.

It also means the launcher does not need a new application-matching algorithm. `app_search::search()` already implements exact-name, prefix, word-prefix, fuzzy, keyword, category, and identifier matching, with recency tie-breaking. Reuse it unchanged except for filtering hidden results and incorporating favorite launch profiles.&#x20;



## 5. Wire the popup into the shell and compositor

This is the most important integration change because the current `Super+Space` implementation has the wrong semantics for your new design.

In `compositor/templar-comp/src/keybindings.rs`, the existing action does this:

```
RecoveryAction::RevealLauncher => {
    self.reveal_home(RevealHome::Show);
    crate::protocols_shell::send_launcher_focus(self);
}
```

Change it to:

```
RecoveryAction::RevealLauncher => {
    crate::protocols_shell::send_launcher_focus(self);
}
```

Now `Super+Space` requests the launcher without hiding or rearranging the user's windows. The shell receives that event through `ShellBridge::take_launcher_focus()` and opens the standalone popup rather than calling `field_screen.focus_search()`.&#x20;



For the surface itself, reuse `ShellApp`'s existing `HostedWindow` implementation and overlay setup used by the bar:

```
let window = event_loop.create_window(
    Window::default_attributes()
        .with_title("Templar Launcher")
        .with_decorations(false)
        .with_transparent(true)
        .with_inner_size(LogicalSize::new(720.0, 480.0))
)?;

bridge.set_layer(&window, Layer::ShellOverlay)?;
bridge.set_exclusive_zone(&window, Edge::Bottom, 0)?;
bridge.set_edge_alignment(
    &window,
    Edge::Bottom,
    Alignment::Centre,
)?;
```

This is the surface-creation portion, not a complete drop-in function. Wrap the window in `HostedWindow`, paint and commit its first frame, then request keyboard focus with the bridge's existing `focus_surface(Some(&window))`.

Your Wayland protocol already supports borrowing keyboard focus for a popup without changing client activation or forcing Home to appear. This is exactly the facility the new launcher needs.&#x20;

On dismissal, call `focus_surface(None)`, drop or unmap the launcher overlay, and leave underlying application activation unchanged. Handle `WindowEvent::Focused(false)` to dismiss when the user clicks elsewhere. Test outside-click focus behavior in the compositor as well; a Wayland client cannot reliably observe clicks delivered to another client's surface.

I would make all launch requests enter one host method, regardless of whether the trigger is `Super+Space`, the bottom bar's search button, or a Homegarden shortcut. The search popup should have only one instance per active seat/output, not three independent implementations.

## 6. Connect Files mode to Index1

Use the existing `SearchSession` client. A minimal adapter looks like this:

```
use anyhow::Result;
use templar_file_index::Query;
use templar_client::file_index::SearchSession;

async fn launcher_file_search(
    connection: &zbus::Connection,
    text: &str,
) -> Result<Vec<(std::path::PathBuf, bool, String)>> {
    let query = Query {
        text: text.to_owned(),
        page_size: 30,
        client_tag: Some("templar-launcher".into()),
        ..Query::default()
    };

    let session = SearchSession::search(connection, &query).await?;

    let hits = session
        .result()
        .hits
        .iter()
        .filter(|hit| hit.present)
        .map(|hit| {
            Ok((
                hit.row.path.native_path()?,
                hit.row.kind == 1, // directory
                hit.row.display.clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    session.close().await?;
    Ok(hits)
}
```

The important details are that an empty `within` list searches all indexed roots, the result retains native Unix path bytes, and the cursor is closed after consuming the results. Keep the existing cancellation and generation handling when integrating this adapter into the shell's worker/update flow.&#x20;

For a blank Files tab, I would show recent folders or files already known to the shell rather than issue an unrestricted empty query across the entire index. Once text is entered, show matching folders and files with path, modification date, and size where available.

Opening a file should use `data::brokered_file_open()`, which already resolves the application through `Associations1` and invokes the brokered launcher. Do not introduce a parallel `xdg-open` or direct process execution path.

Directories should open in the Files application. If the user opened the popup from a particular project or folder, you can later add an explicit scope control such as “Everywhere / This project,” but I would not make contextual scoping the default. The requested Tab behavior is more important than preserving the old contextual-search model.

## 7. Unify the bottom rail without turning it into another launcher

This is where I would be particularly careful about redundancy.

The rail should contain two kinds of objects: live tasks and saved launch targets. They are related, but they are not the same thing.

| Object                 | What it represents                               | Normal click                                             |
| ---------------------- | ------------------------------------------------ | -------------------------------------------------------- |
| Running application    | One or more actual windows                       | Activate the most recent window                          |
| Saved application      | A program you regularly use                      | Activate if running, otherwise launch                    |
| Saved terminal profile | A terminal configured for a particular directory | Activate a matching instance or launch in that directory |
| Window group           | An existing grouped set of windows               | Activate or expand the group                             |
| Launcher control       | The global Apps/Files popup                      | Open the popup                                           |

This preserves the current division between `taskbar.rs`, which already derives authoritative running-task information from `templar_window::task::units()`, and persistent user choices. The current taskbar deliberately keeps a stable first-seen order; I would preserve that spatial consistency.&#x20;



A small addition to the rail's view model is enough:

```
#[derive(Clone, Debug)]
pub enum RailSlot {
    Running(templar_window::task::TaskKey),
    Favorite(FavoriteId),
    Launcher,
}
```

Use a reconciliation pass before drawing. If Firefox is saved as a favorite and also has running windows, it should occupy one chip, not two. The chip can be marked as running. A named terminal profile is an exception because it represents a distinct launch configuration, not simply another copy of Kitty.

I would leave the right-hand system rail conceptually unchanged. It should remain responsible for system status and controls, while the applications rail combines task switching, favorites, and the search entry point.

### Terminal shortcuts are already supported by your launch service

This is a useful discovery: `Launcher1` has a `LaunchInDirectoryV1` method, which establishes the actual process working directory. It is better than constructing commands such as `kitty --directory ...` because the launcher owns argument handling, process identity, and cgroup accounting.&#x20;



Store a favorite as a launch profile, not as a fragile persistent window ID:

```
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub enum FavoriteTarget {
    App {
        app_id: String,
    },
    Terminal {
        app_id: String,
        label: String,
        cwd_b64: String,
        arguments: Vec<String>,
    },
}
```

Then, for a terminal profile:

```
let bytes = templar_client::file_index::decode_path(&cwd_b64)?;
let path = templar_file_index::NativePath::from_bytes(bytes)
    .into_path();

let directory_uri = templar_types::uri_list::encode_path(&path);

proxy
    .launch_in_directory_v1(
        &app_id,
        arguments,
        &directory_uri,
        "",
    )
    .await?;
```

This preserves non-UTF-8 directory paths and avoids interpreting user-provided directory names as shell commands.

It also gives TemplarOS a useful feature: a favorite called “SYS Development” could open a terminal directly in the Save Your Skin repository, and another favorite could open a terminal in the TemplarOS repository. Those remain meaningful even after a reboot. A live terminal window could still be pinned for the current session.

### Homegarden and the rail should have different responsibilities

Homegarden can retain its project and window representations, including parked sessions. The rail is simply a fast navigation surface for those sessions.

The launcher creates things or finds them. The rail gets you back to things. Homegarden shows how they relate.

An optional launcher shortcut on Homegarden should call the same global popup action. It should not embed a second search interface.

## 8. Context menus, renaming, and hiding results

I agree with making Delete mean Hide from search, not filesystem deletion.

The launcher is a discovery tool. Files is the place for actual file manipulation. Homegarden is not a trash-management interface.

I would use these context-menu semantics:

| Menu command    | Application result                         | File or folder result                                       |
| --------------- | ------------------------------------------ | ----------------------------------------------------------- |
| Create shortcut | Pin to rail or add to Home                 | Add a shortcut to Home/project                              |
| Rename          | Change the shortcut's display label        | Rename shortcut, or explicitly “Rename file…” through Files |
| Get info        | App ID, desktop entry, installation source | Full path, file type, size, modified time                   |
| Delete / Hide   | Hide from launcher results                 | Hide from launcher results                                  |
| Restore         | Via Settings → Launcher → Hidden items     | Same                                                        |

For the literal Delete command, I would display Hide from search as the visible menu label, perhaps with the explanation “The application or file will not be deleted.” This avoids surprising users who assume Delete means real removal.

Reuse `surface/context_menu.rs`, including its existing action declarations and confirmation behavior. Add the launcher actions to `templar_types::action::MANIFEST`, with distinct actions for creating shortcuts, editing aliases, showing information, and hiding results. The repository already has `xtask check-actions` to enforce consistency between declarations and pointer routes.

I would not reuse `HomeGarden1.Hide` for this. It hides garden places, not arbitrary indexed file results. Instead, store separate launcher-specific hidden identities:

```
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct LauncherVisibility {
    pub hidden_apps: std::collections::HashSet<String>,
    pub hidden_paths_b64: std::collections::HashSet<String>,
}
```

Filter results after they are supplied by the application catalogue or Index1. That keeps the index unchanged and makes hiding instant and reversible. Expose an Unhide control in Settings. An alias should likewise be launcher metadata rather than a modification to the installed application's `.desktop` file.

One further improvement: update the recent-application record on meaningful activations as well as fresh launches. Currently `RecentApps` primarily tracks launches. That is not quite the same as the applications a person has used most recently, particularly if they keep programs open for days.

## 9. Pruning and reorganizing Settings

The Settings architecture is stronger than a typical collection of unrelated configuration dialogs: there is a typed schema, capability states, and common presentation. The problem is now exposing too many implementation-level decisions as ordinary user preferences.

I found 19 Settings page destinations in `crates/templar-types/src/settings.rs`. I would not remove working functionality, but I would reduce the number of choices presented at the top level.

### Concrete pruning recommendations

| Current setting or area            | Recommendation                                                    | Reason                                                                                         |
| ---------------------------------- | ----------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `shell.dock.grouping`              | Deprecate                                                         | I found schema/test references but no runtime consumer implementing its choices.               |
| `resources.mode`                   | Remove from ordinary Settings until enforced behavior is verified | The schema offers `enforce`, but I did not find a runtime reader of that key in this snapshot. |
| `shell.home-field.throw`           | Move to Advanced or hide                                          | Implemented, but explicitly experimental and off by default.                                   |
| Homegarden half-life, max contexts | Move to Advanced                                                  | Real algorithm controls, not everyday user decisions.                                          |
| Rail gate width and height         | Move to Advanced                                                  | Valuable tuning controls, but not needed to understand gesture navigation.                     |
| Rail lane layout                   | Retain under Advanced                                             | Functional, but the default contiguous behavior should normally suffice.                       |
| Preview update rate                | Advanced, shown only for live previews                            | A dependent setting that serves no purpose when previews are off.                              |
| FSearch provider selection         | Remove                                                            | Replaced by first-party launcher Files mode.                                                   |
| Home search-related controls       | Remove                                                            | There should be no independent Home search interface.                                          |

These are not all nonfunctional settings. Some work and simply belong in Advanced. That distinction matters. The unused `shell.dock.grouping` and apparently unconsumed `resources.mode` are candidates for actual deprecation; Homegarden tuning and rail geometry are candidates for presentation pruning.&#x20;



I would also address route duplication. The System page currently offers volume and brightness controls, while Sound and Displays provide their own more complete surfaces. Rather than duplicate configuration, use the System page as a compact status overview that directs users to the single authoritative control page for each function.

My proposed top-level organization is:

Network & Bluetooth

Sound

Displays

Input

Power

Appearance & Motion

Desktop & Homegarden

Launcher & Files

Applications & Defaults

Advanced & Diagnostics

This is a navigation proposal rather than a requirement to merge every service. For example, Network and Bluetooth should keep independent implementation ownership even if their entry points appear together.

More importantly, add a rule to Settings development: a setting does not appear as enabled until there is a verifiable runtime consumer and a test demonstrating its effect. `Capability::Unsupported` and `Capability::Unavailable` already give you a way to express that accurately.

## 10. Integrating the animations and particle shaders

This part is more substantial than exposing a slider.

The `templar-effects` crate is currently a pure planner and WGSL shader library. The sandbox renders its effects, but `templar-comp` still uses the older window park/unpark animation in `motion.rs`. The README explicitly identifies compositor integration as unfinished.&#x20;



You should integrate only the essence/window transitions now. I would leave `surface.wgsl` and its glass, stone, wood, and other material treatments out of the shipped interface, as requested.

### Particle size

The shader's current default grain size is `1.5` logical pixels, with a supported range of `0.5–24`. The grain size is applied when drawing and does not require recomputing the movement plan. That makes it an excellent real-time adjustment.

Larger particles working better is not particularly surprising from a visual-design standpoint. Very fine particles look like noise at normal desktop viewing distances; larger ones make the direction and transformation legible. The effect may actually look more intentional with fewer perceived details.

I would start with a default around 6 logical pixels, then compare 4, 6, 8, and 12 in the compositor. This is a recommendation based on your preference, not a measured optimum.

### Add just two useful controls

Appearance → Window animations

Window transition

Essence particles

Simple motion

None

Particle size

6 logical px

FineCoarse

Proposed settings controls. They are not connected to the uploaded distro; they should only become active after compositor integration.

Do not expose dust spread, glow strength, edge widths, and shader internals yet. Those are tuning parameters. The user needs a choice of visual behavior and a control for the one property they can readily judge.

The existing `shell.home-field.reduced-motion` should override these animation choices. Do not add a second competing reduced-motion toggle.

A compatible schema addition in `crates/templar-session-core/src/settings.rs` would be:

```
schema.add(SettingSpec {
    key: "shell.appearance.window-transition".into(),
    description: "Window minimize and restore effect".into(),
    default: SettingValue::Choice("essence".into()),
    choices: vec![
        "off".into(),
        "simple".into(),
        "essence".into(),
    ],
    min: None,
    max: None,
    privileged: false,
});

schema.add(SettingSpec {
    key: "shell.appearance.particle-size".into(),
    description: "Essence grain diameter in logical pixels".into(),
    default: SettingValue::Int(6),
    choices: vec![],
    min: Some(1),
    max: Some(24),
    privileged: false,
});
```

In the compositor-side effect renderer:

```
use templar_effects::essence::Style;

let style = Style {
    grain_px: particle_size.clamp(1, 24) as f32,
    ..Style::DEFAULT
};

// Apply the new style to each frame's uniform data.
// Do not recompute the movement plan for a size change.
```

### The actual compositor integration

There is one technical obstacle that I would resolve before spending much effort on Settings UI.

The effects sandbox uses WGSL through a wgpu-style rendering path. Your Smithay compositor uses GLES rendering, and its custom-shader path expects older GLSL ES conventions. The effects README specifically notes the incompatibility between the sandbox's GLSL ES 3.0 translation and Smithay's custom-shader interface.

I would proceed in this order:

1. Add an `Essence` transition state to `compositor/templar-comp/src/motion.rs`, while retaining the existing 200 ms motion as a reliable fallback.
2. Use the existing minimize-target and window geometry information to plan the essence paths. Retain a single plan for a transition, and reverse it with `Run::reverse()` when minimize and restore happen in quick succession.
3. Implement the rendering in the compositor's existing GLES path, either with a verified GLES 3 context or a GLSL ES 1.00-compatible shader version. Avoid GPU-to-CPU readbacks or spinning up a separate wgpu device simply for the animation.
4. Composite the animated window and motes through the normal scene ordering so the pieces pass through visible gaps without appearing over other windows incorrectly.
5. Wire the Settings values through the existing shell-to-compositor settings propagation. If shader initialization fails, fall back automatically to simple motion.

There is also an important performance distinction: changing grain size should be cheap, but rendering essence particles is not free. The sandbox's own GPU timing tables show nontrivial costs, especially on integrated graphics at 4K. I would retain GPU capability fallback even if the default on your own machine becomes Essence.&#x20;



The bar's animated plates are a separate feature from the window-transition shaders. Since you already like the gesture motion, I would avoid changing its feel while this launcher redesign is underway.

## 11. Retire FSearch completely

This is more than deleting one package. I found FSearch still connected through provider policy, the shell, a special search result, and distro packaging.

| Location                                                 | Required cleanup                                                                        |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| `crates/templar-session-core/src/providers.rs`           | Remove FSearch's default file-search provider policy and related tests.                 |
| `shell/templar-shell/src/omnibox.rs`                     | Remove `SEARCH_EVERYWHERE` and the FSearch fallback row.                                |
| `surface/omnibox_screen.rs`                              | Remove the dedicated FSearch footer UI.                                                 |
| `surface/app.rs`                                         | Remove `search_everywhere()` and the `--search` launcher delegation.                    |
| Files context actions                                    | Make Search Here open the new popup in Files mode, scoped to that folder.               |
| `distro/forks/manifest.toml` and `distro/forks/fsearch/` | Remove the fork/package adoption path after verifying no installer dependencies remain. |
| Documentation                                            | Supersede the old Apps-only Home search and external FSearch decisions.                 |

The packaging is not hypothetical: the snapshot contains a pinned FSearch downstream patch and `templaros-fsearch` packaging. It should be deliberately retired rather than left as an undocumented alternative search path.&#x20;



Keep `Files.Index1` itself, its clients, and compatibility interfaces. They are not part of the FSearch dependency you are removing.

## 12. Implementation order and acceptance criteria

I would implement this in four passes, keeping each independently testable.

Pass A — New global launcher

Create `launcher_model.rs` and `launcher_screen.rs`; make `Super+Space` open a genuine overlay; move the existing app search and recent-app functionality there; remove the permanent top search and result-to-tile drag behavior. Validate click, Enter, Escape, Tab, focus restoration, and window activation.

Pass B — Files and context actions

Connect Index1 search, preserve native paths, add shortcut/rename/info/hide actions, and ensure Search Here uses the same popup. Remove FSearch routing and packaging after the replacement works.

Pass C — Favorites and rail integration

Add persistent application favorites, named terminal-directory profiles, rail deduplication, and the Homegarden launcher shortcut. Keep all current TaskKey/window-group behavior intact.

Pass D — Settings and animation polish

Audit unused settings, simplify navigation, connect Essence to the compositor, add the particle-size and transition controls, and retain tested GLES/reduced-motion fallbacks.

The most important regression tests are those that distinguish navigation from creation: opening an application from search must not create a Home shortcut; clicking a running-app chip must not start an unintended duplicate; hiding a file result must not touch the filesystem; and the launcher must function with another application occupying the screen without revealing Home.

I would also add a test that runs `Super+Space` over a maximized application, types a query immediately, presses Tab, selects a file, and opens it through `Associations1`. That one end-to-end test exercises the new integration more effectively than a collection of isolated screenshot tests.

The existing offscreen PixelKit harnesses are suitable for the pure interaction tests. The window focus, layer ordering, and animation rendering still need compositor-level and installed-system validation. I inspected the source snapshot for this review; I have not modified the repository or run the new integration tests.

## Final direction

The important distinction for TemplarOS is between creating or finding a task and returning to a task. Your existing Homegarden and gesture system are already built around that distinction, but the top search and recent-app strip occupy an awkward third position.

A global PixelKit launcher removes that ambiguity. It gives you a first-party reason to retire FSearch, uses the substantial Index1 infrastructure you already have, lets the rail become a useful persistent combination of live tasks and intentional favorites, and leaves Homegarden free to represent actual working environments.

I would make the launcher overlay and direct-click behavior the immediate priority. Once those are correct, the favorite profiles, Settings cleanup, and particle-animation integration fit into a much more coherent desktop rather than becoming additional independent subsystems.
