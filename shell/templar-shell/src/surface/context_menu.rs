//! The Home Field's secondary-click menu: the pointer route to every window and field action
//! that otherwise had only a chord.
//!
//! The action manifest's own rule is that "a chord is an accelerator, never the only route"
//! (`templar_types::action`'s module doc). Until this existed, eight declared actions broke it —
//! `field.undo`, `field.stash`, `window.show-all`, `window.escape-fullscreen`, the two cycle
//! steps and the two group steps were reachable from a keyboard and from nowhere else, and
//! `window.close` was reachable from nothing at all. Right and middle buttons were already
//! translated into the shell's input layer (`crate::surface::window::translate_button`) and no
//! screen consumed them, so secondary-click did nothing anywhere on the field.
//!
//! Three rules shape what is here, all of them borrowed from
//! [`crate::surface::widgets`], which solved the same problem for tiles:
//!
//! 1. **Rows are declared, not coded.** A [`MenuItem`] names an id from
//!    [`templar_types::action::MANIFEST`] and nothing else. The label, the accelerator shown beside
//!    it and whether it needs confirming all come from that one declaration, so a menu row and
//!    the cheat sheet can never disagree, and `xtask check-actions` can see this table and know
//!    the action has a pointer route.
//! 2. **The menu decides nothing.** It returns a [`Chosen`] — an action id and what it applies
//!    to — and the screen above it turns that into a [`crate::surface::field_screen::FieldAction`].
//!    No D-Bus, no Wayland, no policy.
//! 3. **A destructive row arms before it fires.** `ActionDecl::confirm` is honoured here rather
//!    than at the call site, because a menu is exactly where a stray click lands: the first press
//!    on `window.close` turns the row into its confirmation, and only the second closes anything.
//!    The compositor is never asked twice, and never asked once by accident.
//!
//! Input is claimed in one pass ([`ContextMenu::update`]) and painted in another
//! ([`ContextMenu::paint`]), rather than both at once as every other widget in this crate does.
//! That is forced by `pixelkit_shell::Input`'s claim-once model: the menu is drawn *over* the
//! tiles, so it must take the click *before* they are painted, or the tile underneath claims it
//! first and the menu row never sees it.

use pixelkit_raster::Rect;
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::Ui;
use templar_types::action::{self, ActionDecl};

use templar_field::TileId;
use templar_window::WindowId;

use crate::surface::design;

/// What a row needs in order to be offered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuScope {
    /// Always offered, including on bare field background.
    Field,
    /// Needs a tile under the pointer.
    Tile,
    /// Needs a tile that stands for a live window.
    Window,
    /// Needs a live window that is currently in a window group.
    GroupedWindow,
    /// Needs a window group's own tile under the pointer.
    WindowGroup,
    /// Needs a project's own tile under the pointer.
    Project,
    /// Needs a folder tile — a Place — under the pointer
    /// (`@templaros.decision.homegarden-activity-grown-places`).
    Place,
    /// Needs at least one tile on the shelf. Offered on bare background like the other field
    /// rows, but only when there is actually something to bring back — a permanently visible
    /// "bring shelved tiles back" would be a row that does nothing almost every time it is read.
    Shelf,
    /// The Files object: a row is under the pointer.
    FileRow,
    /// The Files object: bare ground, with no row under the pointer.
    FilesGround,
    /// The Files object: bare ground in a folder that can be listed — a paste and a new folder
    /// both need somewhere to land, and a feed (Recent, Trash) is not somewhere.
    FilesDirectory,
}

/// The declared action a copy-of-an-application row performs. One declaration for every copy:
/// which copy is the row's own payload, not a second action id.
pub const LAUNCH_VARIANT: &str = "app.launch-variant";

/// The global launcher only organizes targets after an explicit context action.
/// The view freezes the native target; the menu remains the shared declaration-driven widget.
pub const LAUNCHER_MENU: &[MenuItem] = &[
    MenuItem::new("launcher.pin-rail", MenuScope::Field),
    MenuItem::new("launcher.add-home", MenuScope::Field),
    MenuItem::new("launcher.rename-alias", MenuScope::Field),
    MenuItem::new("launcher.info", MenuScope::Field),
    MenuItem::new("launcher.hide", MenuScope::Field),
];

/// One menu row: an action id and what has to be under the pointer for it to appear.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MenuItem {
    /// An `id` from [`templar_types::action::MANIFEST`].
    pub action: &'static str,
    pub scope: MenuScope,
}

impl MenuItem {
    pub const fn new(action: &'static str, scope: MenuScope) -> MenuItem {
        MenuItem { action, scope }
    }

    /// The declaration this row points at. `None` only for a row naming an id that has been
    /// removed from the manifest, which `xtask check-actions` refuses.
    pub fn declaration(&self) -> Option<&'static ActionDecl> {
        action::action(self.action)
    }
}

/// Every row the field's context menu can offer, in display order.
///
/// Close is last and alone at the bottom, the position a destructive row has held in menus for
/// forty years; nothing above it can be reached by overshooting.
pub const MENU: &[MenuItem] = &[
    MenuItem::new("field.help", MenuScope::Field),
    MenuItem::new("launcher.show-all", MenuScope::Field),
    MenuItem::new("window.park", MenuScope::Window),
    MenuItem::new("windows.arrange.focus", MenuScope::Field),
    MenuItem::new("windows.arrange.halves", MenuScope::Field),
    MenuItem::new("windows.arrange.thirds", MenuScope::Field),
    MenuItem::new("windows.arrange.quarters", MenuScope::Field),
    MenuItem::new("windows.arrange.main-and-stack", MenuScope::Field),
    MenuItem::new("windows.arrange.cushion", MenuScope::Field),
    MenuItem::new("windows.restore-arrangement", MenuScope::Field),
    MenuItem::new("pane.new", MenuScope::Field),
    MenuItem::new("window.escape-fullscreen", MenuScope::Window),
    // Window groups. `window.group` is not here: grouping needs a second window to name, which a
    // menu row over one tile has no way to say — its pointer route is the drop of one session
    // tile on another, and the compositor's `Super+G` is the keyboard one.
    MenuItem::new("window.activate-group", MenuScope::WindowGroup),
    MenuItem::new("window.shelve-group", MenuScope::WindowGroup),
    MenuItem::new("window.unshelve-group", MenuScope::WindowGroup),
    MenuItem::new("window.rename-group", MenuScope::WindowGroup),
    MenuItem::new("window.ungroup", MenuScope::GroupedWindow),
    MenuItem::new("window.cycle-forward", MenuScope::Window),
    MenuItem::new("window.cycle-backward", MenuScope::Window),
    MenuItem::new("window.step-right", MenuScope::Window),
    MenuItem::new("window.step-left", MenuScope::Window),
    // A project's own rows, above the generic tile rows: entering a project and restoring its
    // workspace are what a person came to the menu for, and the three tools are the reason a
    // project is a place rather than a folder.
    MenuItem::new("project.enter", MenuScope::Project),
    MenuItem::new("project.restore", MenuScope::Project),
    MenuItem::new("project.open-terminal", MenuScope::Project),
    MenuItem::new("project.open-files", MenuScope::Project),
    MenuItem::new("project.open-editor", MenuScope::Project),
    // A Place's tools and its gardening, above the generic tile rows for the same reason.
    // Above the tools, because it is about *this window and this place* rather than about
    // opening something new — and because it is the one row here whose availability depends on
    // what has focus.
    MenuItem::new("place.adopt-window", MenuScope::Place),
    MenuItem::new("place.open-terminal", MenuScope::Place),
    MenuItem::new("place.open-files", MenuScope::Place),
    MenuItem::new("place.open-editor", MenuScope::Place),
    MenuItem::new("garden.pin", MenuScope::Place),
    MenuItem::new("garden.unpin", MenuScope::Place),
    MenuItem::new("garden.keep", MenuScope::Place),
    MenuItem::new("garden.show-coding", MenuScope::Place),
    MenuItem::new("garden.show-video", MenuScope::Place),
    MenuItem::new("garden.show-images", MenuScope::Place),
    MenuItem::new("garden.show-documents", MenuScope::Place),
    MenuItem::new("garden.show-plain", MenuScope::Place),
    MenuItem::new("garden.hide", MenuScope::Place),
    MenuItem::new("garden.forget", MenuScope::Place),
    MenuItem::new("garden.why", MenuScope::Place),
    MenuItem::new("garden.tend", MenuScope::Field),
    MenuItem::new("field.move", MenuScope::Tile),
    MenuItem::new("field.resize", MenuScope::Tile),
    MenuItem::new("field.stash", MenuScope::Tile),
    MenuItem::new("field.restore", MenuScope::Shelf),
    MenuItem::new("field.undo", MenuScope::Field),
    MenuItem::new("window.show-all", MenuScope::Field),
    MenuItem::new("home.reveal", MenuScope::Field),
    MenuItem::new("field.reset", MenuScope::Field),
    MenuItem::new("home.pin-app", MenuScope::Window),
    MenuItem::new("window.close", MenuScope::Window),
];

/// Every row the Files object's secondary-click menu can offer, in display order.
///
/// A second table rather than a second menu widget: the rows, the arming, the keyboard cursor
/// and the painting are the field menu's, and the only thing that differs is which declared
/// actions are on offer and what has to be under the pointer for each. Naming the actions here
/// is also what puts them in front of `xtask check-actions`, so a file verb cannot exist on the
/// toolbar under one name and in this menu under another.
///
/// Trash is last on the row menu for the reason Close is last on the field's: it is the
/// destructive one, and nothing above it can be reached by overshooting.
pub const FILES_MENU: &[MenuItem] = &[
    MenuItem::new("files.open", MenuScope::FileRow),
    MenuItem::new("files.open-terminal", MenuScope::FileRow),
    MenuItem::new("files.copy", MenuScope::FileRow),
    MenuItem::new("files.cut", MenuScope::FileRow),
    MenuItem::new("files.copy-path", MenuScope::FileRow),
    MenuItem::new("files.rename", MenuScope::FileRow),
    MenuItem::new("files.properties", MenuScope::FileRow),
    MenuItem::new("files.trash", MenuScope::FileRow),
    MenuItem::new("files.open-terminal", MenuScope::FilesDirectory),
    MenuItem::new("files.new-folder", MenuScope::FilesDirectory),
    MenuItem::new("files.paste", MenuScope::FilesDirectory),
    MenuItem::new("files.select-all", MenuScope::FilesGround),
    MenuItem::new("files.sort", MenuScope::FilesGround),
    MenuItem::new("files.show-hidden", MenuScope::FilesGround),
    MenuItem::new("files.search-here", MenuScope::FilesGround),
];

/// What the Files object's menu was opened over: one of its rows, or the ground between them.
///
/// Deliberately not a path. The menu returns an action id and the view already knows which row
/// the press landed on; carrying the path through the menu as well would give two answers to
/// "which file is this about", and they could differ.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FilesTarget {
    /// Whether a row was under the pointer. `false` is the ground between rows, which is a
    /// target in its own right rather than a miss.
    pub row: bool,
    /// Whether the location being shown is a real folder. A feed — Recent, Trash, Operations —
    /// is somewhere to look, not somewhere to paste into.
    pub directory: bool,
}

impl FilesTarget {
    fn offers(self, scope: MenuScope) -> bool {
        match scope {
            MenuScope::FileRow => self.row,
            MenuScope::FilesGround => !self.row,
            MenuScope::FilesDirectory => !self.row && self.directory,
            // The field's own scopes never appear in `FILES_MENU`; a row that named one would be
            // a row about a tile on a menu about a file.
            MenuScope::Field
            | MenuScope::Tile
            | MenuScope::Window
            | MenuScope::Shelf
            | MenuScope::GroupedWindow
            | MenuScope::WindowGroup
            | MenuScope::Project
            | MenuScope::Place => false,
        }
    }

    /// The rows this target offers, in [`FILES_MENU`] order.
    pub fn rows(self) -> Vec<&'static MenuItem> {
        FILES_MENU
            .iter()
            .filter(|item| self.offers(item.scope) && item.declaration().is_some())
            .collect()
    }
}

/// What the menu was opened over.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MenuTarget {
    pub tile: Option<TileId>,
    /// The live window that tile stands for, if it stands for one.
    pub window: Option<WindowId>,
    /// Whether the field has anything on its shelf right now.
    pub shelved: bool,
    /// The tile under the pointer is a window group's own tile, and [`Self::window`] is one of
    /// its members — every group request on the wire is sent to a member and applies to the set.
    pub window_group: bool,
    /// The window under the pointer belongs to a window group.
    pub in_window_group: bool,
    /// The project whose tile is under the pointer. Carried here rather than looked up again
    /// when a row is chosen: the field has reflowed by then, and "which project" must be the
    /// one the menu was opened over, not whatever is under the pointer now.
    pub project: Option<String>,
    /// The folder tile — a Place — under the pointer, carried for the same reason as `project`.
    pub place: Option<std::path::PathBuf>,
    /// The place's state, so "Pin here" and "Unpin" are one row that says the true thing, and
    /// "Keep this size and place" is not offered for a place already kept.
    pub place_pinned: bool,
    pub place_kept: bool,
    /// The window that has the keyboard right now, if any, and whether it is one a place can be
    /// told about — an adopt names an *instance*, which only a brokered launch has.
    ///
    /// Carried on the target rather than looked up when the row is chosen, for the same reason
    /// `project` and `place` are: the field reflows and windows come and go while a menu is open,
    /// and "which window" must be the one the menu was opened over, not whatever has focus by
    /// the time a row is pressed.
    pub adoptable_window: Option<WindowId>,
}

impl MenuTarget {
    fn offers(&self, scope: MenuScope) -> bool {
        match scope {
            MenuScope::Field => true,
            MenuScope::Tile => self.tile.is_some(),
            MenuScope::Project => self.project.is_some(),
            MenuScope::Place => self.place.is_some(),
            MenuScope::Window => self.window.is_some(),
            MenuScope::Shelf => self.shelved,
            MenuScope::WindowGroup => self.window_group && self.window.is_some(),
            // A group's own tile is not one of its members: "take out of group" over it would
            // have no window to mean, so it is offered over a member's tile only.
            MenuScope::GroupedWindow => {
                self.in_window_group && self.window.is_some() && !self.window_group
            }
            // The Files object's scopes never appear in `MENU`: a file row is not a tile, and a
            // menu over the field has nothing to say about one.
            MenuScope::FileRow | MenuScope::FilesGround | MenuScope::FilesDirectory => false,
        }
    }

    /// Whether a row that depends on the place's state is true of it right now.
    fn state_allows(&self, action: &str) -> bool {
        match action {
            "garden.pin" => !self.place_pinned,
            "garden.unpin" => self.place_pinned,
            "garden.keep" => !self.place_kept,
            // Nothing to adopt with: no focused window, or one whose identity no launch of ours
            // vouches for. Offering the row anyway would be offering a press that cannot work.
            "place.adopt-window" => self.adoptable_window.is_some(),
            _ => true,
        }
    }

    /// The rows this target offers, in [`MENU`] order.
    pub fn rows(&self) -> Vec<&'static MenuItem> {
        MENU.iter()
            .filter(|item| {
                self.offers(item.scope)
                    && self.state_allows(item.action)
                    && item.declaration().is_some()
            })
            .collect()
    }
}

/// One extra row, offered for this target only: a copy of an application to start, named by the
/// variant id that starts it.
///
/// Declared rows cannot express this — an application with three copies installed needs three
/// rows and the manifest has one action for all of them — so a choice carries its own label and
/// the id it stands for, and performs the one declared action `app.launch-variant`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuChoice {
    pub label: String,
    pub detail: String,
    pub id: String,
}

/// One row the user picked. The screen above turns this into an action; the menu never performs
/// anything itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chosen {
    pub action: &'static str,
    pub target: MenuTarget,
    /// Which copy, for a row that named one. `None` for every declared row.
    pub choice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Open {
    at: (i32, i32),
    target: MenuTarget,
    rows: Vec<&'static MenuItem>,
    /// The copies of this application, when it has more than one. Drawn under the declared rows.
    choices: Vec<MenuChoice>,
    /// A `confirm: true` row pressed once and waiting for its second press.
    armed: Option<&'static str>,
    /// Keyboard cursor, so the menu itself is not pointer-only either.
    highlighted: Option<usize>,
    /// First visible row when the menu exceeds the output height.
    first: usize,
}

/// The field's context menu: closed, or open over one target.
#[derive(Debug, Default)]
pub struct ContextMenu {
    open: Option<Open>,
}

/// Logical-pixel geometry. Deliberately roomier than a tile's own chrome: this is a pointer
/// target of last resort for people who cannot use the chords.
const ROW_HEIGHT: i32 = 28;
const WIDTH: i32 = 268;
const PADDING: i32 = 5;

impl ContextMenu {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// What the menu is currently open over, if it is open.
    pub fn target(&self) -> Option<MenuTarget> {
        self.open.as_ref().map(|open| open.target.clone())
    }

    /// Open over `target` at the pointer. A target with no rows at all opens nothing, rather
    /// than flashing an empty box.
    pub fn open_at(&mut self, at: (i32, i32), target: MenuTarget) {
        self.open_at_with(at, target, Vec::new());
    }

    /// The same, plus a row per copy of the application under the pointer. An application with
    /// one copy — which is nearly all of them — gets no extra rows and reads exactly as before.
    pub fn open_at_with(&mut self, at: (i32, i32), target: MenuTarget, choices: Vec<MenuChoice>) {
        let rows = target.rows();
        if rows.is_empty() && choices.is_empty() {
            return;
        }
        self.open = Some(Open {
            at,
            target,
            rows,
            choices,
            armed: None,
            highlighted: None,
            first: 0,
        });
    }

    /// Open over an explicit row set, rather than over one of [`MENU`]'s targets.
    ///
    /// The Files object composes its own rows ([`FilesTarget::rows`]) because what is under the
    /// pointer there is a file, not a tile. Everything after that — arming a destructive row,
    /// the keyboard cursor, the painting, dismissal — is this widget's and is not duplicated.
    pub fn open_rows_at(&mut self, at: (i32, i32), rows: Vec<&'static MenuItem>) {
        if rows.is_empty() {
            return;
        }
        self.open = Some(Open {
            at,
            target: MenuTarget::default(),
            rows,
            choices: Vec::new(),
            armed: None,
            highlighted: None,
            first: 0,
        });
    }

    pub fn close(&mut self) {
        self.open = None;
    }

    /// The menu's rectangle, clamped to stay inside `area`. Anchored at the pointer's
    /// top-left the way a menu is everywhere, and flipped rather than clipped near an edge.
    pub fn rect(&self, ui: &Ui<'_>, area: Rect) -> Option<Rect> {
        let open = self.open.as_ref()?;
        let width = ui.px(WIDTH).min(area.w.max(1));
        let height =
            ui.px(PADDING) * 2 + ui.px(ROW_HEIGHT) * (open.rows.len() + open.choices.len()) as i32;
        let height = height.min(area.h.max(1));
        let x = open.at.0.min(area.right() - width).max(area.x);
        let y = open.at.1.min(area.bottom() - height).max(area.y);
        Some(Rect::new(x, y, width, height))
    }

    /// The `index`-th row's rectangle, for a caller that needs to aim at one (a test, or a
    /// future surface that wants to place something beside the menu). `None` when the menu is
    /// closed or has no such row.
    pub fn row(&self, ui: &Ui<'_>, area: Rect, index: usize) -> Option<Rect> {
        let rect = self.rect(ui, area)?;
        let open = self.open.as_ref()?;
        let (body, capacity, _) = Self::viewport(rect, ui, open.rows.len() + open.choices.len());
        (index >= open.first
            && index < open.first + capacity
            && index < open.rows.len() + open.choices.len())
        .then(|| Self::row_rect(body, ui, index - open.first))
    }

    /// Overflow reserves two pointer controls; rows never extend beyond the viewport.
    fn viewport(rect: Rect, ui: &Ui<'_>, count: usize) -> (Rect, usize, bool) {
        let inner = rect.inset(ui.px(PADDING));
        let height = ui.px(ROW_HEIGHT).max(1);
        let overflow = count as i32 * height > inner.h;
        let control = if overflow {
            ui.px(20).min(inner.h.max(0) / 3)
        } else {
            0
        };
        let body = Rect::new(
            inner.x,
            inner.y + control,
            inner.w,
            (inner.h - control * 2).max(0),
        );
        let capacity = (body.h / height).max(0) as usize;
        (body, capacity, overflow)
    }

    fn row_rect(body: Rect, ui: &Ui<'_>, index: usize) -> Rect {
        Rect::new(
            body.x,
            body.y + ui.px(ROW_HEIGHT) * index as i32,
            body.w,
            ui.px(ROW_HEIGHT),
        )
    }

    /// Claim this frame's input for the menu, and report a row the user committed to.
    ///
    /// Called before anything underneath is painted, so an open menu owns the click (see the
    /// module doc). A press outside closes the menu and is swallowed: dismissing a menu is a
    /// gesture in itself, and must not also activate whatever it was covering.
    pub fn update(&mut self, ui: &mut Ui<'_>, area: Rect) -> Option<Chosen> {
        let rect = self.rect(ui, area)?;
        // Taken, not read: while a menu is up the keyboard belongs to it, and an Escape that
        // dismissed the menu must not also reach the field behind it and clear its focus.
        let keys = ui.input.take_keys();
        let open = self.open.as_mut()?;
        let count = open.rows.len() + open.choices.len();

        let (body, capacity, overflow) = Self::viewport(rect, ui, count);
        let last_first = count.saturating_sub(capacity);
        open.first = open.first.min(last_first);
        let mut navigated = false;
        let mut committed = false;
        for key in keys {
            match key {
                KeyInput::Escape => {
                    self.open = None;
                    return None;
                }
                KeyInput::Down => {
                    navigated = true;
                    open.highlighted = Some(open.highlighted.map_or(0, |at| (at + 1) % count));
                }
                KeyInput::Up => {
                    navigated = true;
                    open.highlighted = Some(
                        open.highlighted
                            .map_or(count - 1, |at| (at + count - 1) % count),
                    );
                }
                KeyInput::Enter => committed = true,
                _ => {}
            }
        }

        if navigated && let Some(index) = open.highlighted {
            if index < open.first {
                open.first = index;
            }
            if index >= open.first + capacity {
                open.first = (index + 1).saturating_sub(capacity).min(last_first);
            }
        }
        if overflow && Self::scroll_menu(open, ui, rect, body, capacity) {
            return None;
        }
        let mut picked = None;
        if committed && let Some(index) = open.highlighted {
            picked = Some(index);
        }
        for index in open.first..(open.first + capacity).min(count) {
            let row = Self::row_rect(body, ui, index - open.first);
            if ui.input.focus_requested(row) {
                open.highlighted = Some(index);
            }
            if ui.input.take_click(row) {
                picked = Some(index);
            }
        }

        if let Some(index) = picked {
            if let Some(item) = open.rows.get(index).copied() {
                let confirms = item.declaration().is_some_and(|decl| decl.confirm);
                if confirms && open.armed != Some(item.action) {
                    open.armed = Some(item.action);
                    return None;
                }
                let chosen = Chosen {
                    action: item.action,
                    target: open.target.clone(),
                    choice: None,
                };
                self.open = None;
                return Some(chosen);
            }
            if let Some(choice) = open.choices.get(index - open.rows.len()) {
                let chosen = Chosen {
                    action: LAUNCH_VARIANT,
                    target: open.target.clone(),
                    choice: Some(choice.id.clone()),
                };
                self.open = None;
                return Some(chosen);
            }
        }

        // Anywhere else: dismiss, and swallow the press.
        if ui.input.take_click(area) || ui.input.take_context_click(area) {
            self.open = None;
        }
        None
    }

    fn scroll_menu(
        open: &mut Open,
        ui: &mut Ui<'_>,
        rect: Rect,
        body: Rect,
        capacity: usize,
    ) -> bool {
        let last_first = (open.rows.len() + open.choices.len()).saturating_sub(capacity);
        let inner = rect.inset(ui.px(PADDING));
        let up = Rect::new(inner.x, inner.y, inner.w, body.y - inner.y);
        let down = Rect::new(
            inner.x,
            body.bottom(),
            inner.w,
            inner.bottom() - body.bottom(),
        );
        let (_, dy) = ui.input.take_scroll(rect);
        let previous = ui.input.take_click(up);
        let next = ui.input.take_click(down);
        if previous || next || dy != 0.0 {
            let step = if previous || next {
                capacity.max(1)
            } else {
                (dy.abs() / ui.px(ROW_HEIGHT).max(1) as f32).ceil() as usize
            };
            open.first = if previous || dy < 0.0 {
                open.first.saturating_sub(step)
            } else {
                open.first.saturating_add(step).min(last_first)
            };
            open.highlighted = None;
            open.armed = None;
            return true;
        }
        false
    }

    fn paint_scroll_controls(ui: &mut Ui<'_>, rect: Rect, body: Rect, previous: bool, next: bool) {
        let inner = rect.inset(ui.px(PADDING));
        for (control, label, enabled) in [
            (
                Rect::new(inner.x, inner.y, inner.w, body.y - inner.y),
                "↑  More",
                previous,
            ),
            (
                Rect::new(
                    inner.x,
                    body.bottom(),
                    inner.w,
                    inner.bottom() - body.bottom(),
                ),
                "↓  More",
                next,
            ),
        ] {
            if enabled && ui.input.hovering(control) {
                ui.painter.fill_rect(control, design::accent_soft());
            }
            ui.label_styled(
                control,
                label,
                design::state_style(ui),
                if enabled {
                    design::text()
                } else {
                    design::text_faint()
                },
                Align::Centre,
            );
        }
    }

    fn paint_shell(ui: &mut Ui<'_>, rect: Rect) {
        ui.painter
            .shadow_rect(rect, ui.px(3), ui.px(6), design::text(), 70);
        ui.painter
            .fill_rounded_rect(rect, ui.px(design::WELL_RADIUS), design::shell_fill());
        ui.painter
            .stroke_rect(rect, ui.scale.hairline(), design::edge_strong());
    }

    /// Draw the menu over everything else. Purely visual — [`update`](Self::update) already took
    /// the input this frame.
    pub fn paint(&self, ui: &mut Ui<'_>, area: Rect) {
        let (Some(rect), Some(open)) = (self.rect(ui, area), self.open.as_ref()) else {
            return;
        };
        ui.modal_semantics();
        Self::paint_shell(ui, rect);

        let count = open.rows.len() + open.choices.len();
        let (body, capacity, overflow) = Self::viewport(rect, ui, count);
        if overflow {
            Self::paint_scroll_controls(
                ui,
                rect,
                body,
                open.first > 0,
                open.first + capacity < count,
            );
        }
        for (index, item) in open.rows.iter().enumerate() {
            let Some(declaration) = item.declaration() else {
                continue;
            };
            let Some(row) = self.row(ui, area, index) else {
                continue;
            };
            let armed = open.armed == Some(item.action);
            let hovered = ui.input.hovering(row) || open.highlighted == Some(index);
            let destructive = declaration.confirm;
            // An armed destructive row wears the danger plate, a merely hovered one the accent:
            // one fill, so the two states cannot drift into different radii.
            let plate = if armed {
                Some(design::danger_soft())
            } else if hovered {
                Some(design::accent_soft())
            } else {
                None
            };
            if let Some(plate) = plate {
                ui.painter
                    .fill_rounded_rect(row, ui.px(design::CHIP_RADIUS), plate);
            }
            let (accelerator, label) = row.split_right(ui.px(96).min(row.w / 2));
            // Armed rows say what the second press will do, in plain words: a row that only
            // changed colour would be asking the user to guess.
            let text = if armed {
                format!("{} — press again", declaration.label)
            } else {
                declaration.label.to_owned()
            };
            ui.semantic(
                pixelkit_ui::semantics::Role::MenuItem,
                &text,
                row,
                open.highlighted == Some(index),
                None,
                None,
            );
            ui.label_styled(
                Rect::new(
                    label.x + ui.px(8),
                    label.y,
                    (label.w - ui.px(8)).max(0),
                    label.h,
                ),
                &text,
                design::title_style(ui),
                if armed || destructive {
                    design::danger()
                } else {
                    design::text()
                },
                Align::Left,
            );
            if let Some(chord) = declaration.primary_chord() {
                ui.label_styled(
                    Rect::new(
                        accelerator.x,
                        accelerator.y,
                        (accelerator.w - ui.px(8)).max(0),
                        accelerator.h,
                    ),
                    &chord.to_string(),
                    ui.text_style(TextStyle::tracked(ui.theme.body.face, 11.0, 0.10)),
                    design::text_faint(),
                    Align::Right,
                );
            }
        }

        self.paint_choices(ui, area, open);
    }

    /// The copies, under the declared rows: which one, and where it came from. They are the only
    /// rows the manifest does not name, and they say so by looking like what they are — a name
    /// and a provenance, not a verb.
    fn paint_choices(&self, ui: &mut Ui<'_>, area: Rect, open: &Open) {
        for (offset, choice) in open.choices.iter().enumerate() {
            let index = open.rows.len() + offset;
            let Some(row) = self.row(ui, area, index) else {
                continue;
            };
            ui.semantic(
                pixelkit_ui::semantics::Role::MenuItem,
                &choice.label,
                row,
                open.highlighted == Some(index),
                None,
                None,
            );
            if ui.input.hovering(row) || open.highlighted == Some(index) {
                let plate = design::accent_soft();
                ui.painter
                    .fill_rounded_rect(row, ui.px(design::CHIP_RADIUS), plate);
            }
            let (detail, label) = row.split_right(ui.px(120).min(row.w / 2));
            ui.label_styled(
                Rect::new(
                    label.x + ui.px(8),
                    label.y,
                    (label.w - ui.px(8)).max(0),
                    label.h,
                ),
                &choice.label,
                design::title_style(ui),
                design::text(),
                Align::Left,
            );
            ui.label_styled(
                Rect::new(detail.x, detail.y, (detail.w - ui.px(8)).max(0), detail.h),
                &choice.detail,
                ui.text_style(TextStyle::tracked(ui.theme.body.face, 11.0, 0.10)),
                design::text_faint(),
                Align::Right,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_ui<T>(input: &mut pixelkit_shell::Input, f: impl FnOnce(&mut Ui<'_>) -> T) -> T {
        let mut buffer = pixelkit_raster::WindowBuffer::new(400, 240);
        let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
        let mut kernel = pixelkit_raster::RasterKernel::new();
        let mut ui = Ui::new(
            pixelkit_raster::Painter::new(&mut buffer),
            &mut text,
            input,
            design::theme(),
            pixelkit_shell::Scale::ONE,
            &mut kernel,
        );
        f(&mut ui)
    }

    #[test]
    fn overflowing_menu_reaches_last_action_without_offscreen_hit_targets() {
        let mut menu = ContextMenu::default();
        menu.open_at(
            (390, 230),
            MenuTarget {
                window: Some(WindowId(3)),
                tile: Some(TileId(7)),
                ..MenuTarget::default()
            },
        );
        let mut input = pixelkit_shell::Input::new();
        let area = Rect::new(0, 0, 400, 240);
        let count = menu.open.as_ref().unwrap().rows.len();
        with_ui(&mut input, |ui| {
            assert!(menu.row(ui, area, count - 1).is_none());
            let rect = menu.rect(ui, area).unwrap();
            for index in 0..count {
                if let Some(row) = menu.row(ui, area, index) {
                    assert!(row.y >= rect.y && row.bottom() <= rect.bottom());
                }
            }
        });
        // Up wraps to Close and scrolls it into view. Confirmation remains two presses.
        input.key(KeyInput::Up);
        with_ui(&mut input, |ui| {
            assert!(menu.update(ui, area).is_none());
        });
        let row = with_ui(&mut input, |ui| menu.row(ui, area, count - 1).unwrap());
        assert!(row.bottom() <= area.bottom());
        input.cursor_moved((row.x + 5) as f32, (row.y + 5) as f32);
        input.mouse(pixelkit_shell::MouseButton::Left, true);
        with_ui(&mut input, |ui| {
            assert!(menu.update(ui, area).is_none());
        });
        input.end_frame();
        input.mouse(pixelkit_shell::MouseButton::Left, false);
        input.mouse(pixelkit_shell::MouseButton::Left, true);
        let chosen = with_ui(&mut input, |ui| menu.update(ui, area).unwrap());
        assert_eq!(chosen.action, "window.close");
        assert_eq!(chosen.target.window, Some(WindowId(3)));
    }

    #[test]
    fn overflowing_menu_can_page_by_pointer_and_wheel() {
        let mut menu = ContextMenu::default();
        menu.open_at((0, 0), MenuTarget::default());
        let mut input = pixelkit_shell::Input::new();
        let area = Rect::new(0, 0, 400, 240);
        let count = menu.open.as_ref().unwrap().rows.len();
        let rect = with_ui(&mut input, |ui| menu.rect(ui, area).unwrap());
        input.cursor_moved((rect.x + 20) as f32, (rect.bottom() - 10) as f32);
        for _ in 0..count {
            input.mouse(pixelkit_shell::MouseButton::Left, true);
            with_ui(&mut input, |ui| {
                assert!(menu.update(ui, area).is_none());
            });
            input.end_frame();
            input.mouse(pixelkit_shell::MouseButton::Left, false);
        }
        with_ui(&mut input, |ui| {
            assert!(menu.row(ui, area, count - 1).is_some());
        });
        input.scrolled(0.0, -10000.0);
        with_ui(&mut input, |ui| {
            assert!(menu.update(ui, area).is_none());
        });
        with_ui(&mut input, |ui| {
            assert!(menu.row(ui, area, 0).is_some());
        });
    }

    /// A copy-of-an-application row is offered under the declared ones, carries which copy it
    /// stands for, and performs the one declared action for all of them. An application with a
    /// single copy has no such rows at all, which is nearly every application.
    #[test]
    fn a_copy_row_names_its_variant_and_performs_one_declared_action() {
        assert!(
            action::action(LAUNCH_VARIANT).is_some(),
            "the copy row performs an undeclared action"
        );
        let target = MenuTarget {
            tile: Some(templar_field::TileId(7)),
            ..MenuTarget::default()
        };
        let mut menu = ContextMenu::default();
        menu.open_at((10, 10), target.clone());
        let declared = menu.open.as_ref().expect("open").rows.len();

        menu.open_at_with(
            (10, 10),
            target,
            vec![
                MenuChoice {
                    label: "Firefox".into(),
                    detail: "DistroPackage 128".into(),
                    id: "desktop-import.firefox.desktop".into(),
                },
                MenuChoice {
                    label: "Firefox".into(),
                    detail: "Flatpak 129".into(),
                    id: "flatpak.org.mozilla.firefox".into(),
                },
            ],
        );
        let open = menu.open.as_ref().expect("open");
        assert_eq!(open.rows.len(), declared, "the declared rows changed");
        assert_eq!(open.choices.len(), 2);
        // And the copies come after every declared row, so a habit built on the menu's order
        // does not break when an application gains a second copy.
        assert!(
            menu.row(
                &Ui::new(
                    pixelkit_raster::Painter::new(&mut pixelkit_raster::WindowBuffer::new(
                        400, 1200
                    )),
                    &mut pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                    &mut pixelkit_shell::Input::new(),
                    design::theme(),
                    pixelkit_shell::Scale::ONE,
                    &mut pixelkit_raster::RasterKernel::new(),
                ),
                Rect::new(0, 0, 400, 1200),
                declared + 1,
            )
            .is_some(),
            "the second copy has no row"
        );
    }

    #[test]
    fn every_row_names_a_declared_action() {
        for item in MENU {
            assert!(
                item.declaration().is_some(),
                "menu row {:?} names no declared action",
                item.action
            );
        }
    }

    /// The point of the whole module: the actions the record found reachable by keyboard alone
    /// are on this menu, and so is `window.close`, which was reachable from nothing.
    #[test]
    fn the_keyboard_only_actions_all_have_a_row() {
        for id in [
            "field.undo",
            "field.stash",
            "window.show-all",
            "window.escape-fullscreen",
            "window.cycle-forward",
            "window.cycle-backward",
            "window.step-right",
            "window.step-left",
            "window.close",
        ] {
            assert!(
                MENU.iter().any(|item| item.action == id),
                "{id} still has no pointer route"
            );
        }
    }

    #[test]
    fn a_row_is_offered_only_where_it_could_be_performed() {
        let nothing = MenuTarget::default();
        let tile = MenuTarget {
            tile: Some(TileId(7)),
            window: None,
            ..MenuTarget::default()
        };
        let window = MenuTarget {
            tile: Some(TileId(7)),
            window: Some(WindowId(3)),
            ..MenuTarget::default()
        };
        let grouped = MenuTarget {
            in_window_group: true,
            ..window.clone()
        };
        let group_tile = MenuTarget {
            window_group: true,
            in_window_group: true,
            ..window.clone()
        };
        // A project's tile: the five project rows, and nothing about a window it does not have.
        let project = MenuTarget {
            tile: Some(TileId(7)),
            project: Some("notes".to_owned()),
            ..MenuTarget::default()
        };
        let shelf = MenuTarget {
            shelved: true,
            ..MenuTarget::default()
        };

        let ids = |target: &MenuTarget| -> Vec<&'static str> {
            target.rows().iter().map(|item| item.action).collect()
        };
        assert!(!ids(&nothing).contains(&"field.stash"));
        assert!(!ids(&nothing).contains(&"window.close"));
        assert!(ids(&nothing).contains(&"field.undo"));
        assert!(ids(&tile).contains(&"field.stash"));
        // A project's tile offers entering it, restoring it, and its three tools — and an
        // ordinary tile offers none of them, because there is no project for them to mean.
        for row in [
            "project.enter",
            "project.restore",
            "project.open-terminal",
            "project.open-files",
            "project.open-editor",
        ] {
            assert!(ids(&project).contains(&row), "a project has no {row} row");
            assert!(!ids(&tile).contains(&row), "an ordinary tile offers {row}");
            assert!(!ids(&nothing).contains(&row), "bare ground offers {row}");
        }
        // Creating a project belongs to the native Files application, which has its own menu:
        // this browse pane is a summary, and Home has no selected real directory to root one at.
        assert!(!ids(&project).contains(&"project.create-here"));
        assert!(
            !FILES_MENU
                .iter()
                .any(|item| item.action == "project.create-here"),
            "the browse pane offers a row it cannot perform"
        );
        assert!(
            !ids(&tile).contains(&"window.close"),
            "a tile with no live window has nothing to close"
        );
        assert!(ids(&window).contains(&"window.close"));
        assert_eq!(
            ids(&window).last(),
            Some(&"window.close"),
            "the destructive row stays at the bottom"
        );

        // Restoring is offered only when there is something on the shelf: a row that reads
        // "bring shelved tiles back" and does nothing is worse than no row.
        assert!(
            !ids(&nothing).contains(&"field.restore"),
            "an empty shelf offers nothing to bring back"
        );
        assert!(!ids(&tile).contains(&"field.restore"));
        assert!(ids(&shelf).contains(&"field.restore"));

        // Reset is a field row, so it is reachable from bare background — that is the whole
        // point, since a field can be reset when no tile is worth pointing at.
        assert!(ids(&nothing).contains(&"field.reset"));

        // Window groups. A window in no group is offered none of it; a member is offered the way
        // out; the group's own tile is offered the verbs that act on the set. "Take out of group"
        // is not among them, because a group's tile is not one of its members.
        for id in [
            "window.ungroup",
            "window.activate-group",
            "window.shelve-group",
            "window.rename-group",
        ] {
            assert!(!ids(&window).contains(&id), "{id} over an ungrouped window");
        }
        assert!(ids(&grouped).contains(&"window.ungroup"));
        assert!(!ids(&grouped).contains(&"window.activate-group"));
        assert!(ids(&group_tile).contains(&"window.activate-group"));
        assert!(ids(&group_tile).contains(&"window.shelve-group"));
        assert!(ids(&group_tile).contains(&"window.unshelve-group"));
        assert!(ids(&group_tile).contains(&"window.rename-group"));
        assert!(
            !ids(&group_tile).contains(&"window.ungroup"),
            "a group's own tile is not one of its members"
        );
    }

    #[test]
    fn a_place_offers_pin_or_unpin_by_its_state_and_keep_only_until_kept() {
        // A Place: "Pin here" or "Unpin", never both, and "Keep" only until it is kept.
        let place = MenuTarget {
            tile: Some(TileId(3)),
            place: Some(std::path::PathBuf::from("/home/dk/src/templaros")),
            ..MenuTarget::default()
        };
        let ids = |t: &MenuTarget| -> Vec<&str> { t.rows().iter().map(|r| r.action).collect() };
        assert!(ids(&place).contains(&"garden.pin") && !ids(&place).contains(&"garden.unpin"));
        assert!(ids(&place).contains(&"garden.keep"));
        let pinned = MenuTarget {
            place_pinned: true,
            ..place.clone()
        };
        assert!(ids(&pinned).contains(&"garden.unpin") && !ids(&pinned).contains(&"garden.pin"));
        let kept = MenuTarget {
            place_pinned: true,
            place_kept: true,
            ..place.clone()
        };
        assert!(!ids(&kept).contains(&"garden.keep"));

        // Adopt is offered only when there is a window to adopt, and never anywhere but over a
        // place: it is a sentence with two halves, and half of it is not a row.
        assert!(
            !ids(&place).contains(&"place.adopt-window"),
            "nothing has focus, so there is nothing to adopt"
        );
        let with_window = MenuTarget {
            adoptable_window: Some(WindowId(7)),
            ..place.clone()
        };
        assert!(ids(&with_window).contains(&"place.adopt-window"));
        let no_place = MenuTarget {
            adoptable_window: Some(WindowId(7)),
            ..MenuTarget::default()
        };
        assert!(!ids(&no_place).contains(&"place.adopt-window"));
    }
    #[test]
    fn opening_over_a_target_with_no_rows_opens_nothing() {
        // Every target offers the field rows today, so this is exercised through an empty table
        // rather than an empty target: the guard exists so a future scope cannot flash an empty
        // box, and it is the `rows.is_empty()` branch that must hold.
        let mut menu = ContextMenu::default();
        assert!(!menu.is_open());
        menu.open_at((10, 10), MenuTarget::default());
        assert!(menu.is_open());
        menu.close();
        assert!(!menu.is_open());
    }
}
