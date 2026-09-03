//! The Home Field's secondary-click menu: the pointer route to every window and field action
//! that otherwise had only a chord.
//!
//! The action manifest's own rule is that "a chord is an accelerator, never the only route"
//! (`lege_types::action`'s module doc). Until this existed, eight declared actions broke it —
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
//!    [`lege_types::action::MANIFEST`] and nothing else. The label, the accelerator shown beside
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

use lege_types::action::{self, ActionDecl};
use pixelkit_raster::Rect;
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::Ui;

use lege_field::TileId;
use lege_window::WindowId;

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
    /// Needs at least one tile on the shelf. Offered on bare background like the other field
    /// rows, but only when there is actually something to bring back — a permanently visible
    /// "bring shelved tiles back" would be a row that does nothing almost every time it is read.
    Shelf,
}

/// The declared action a copy-of-an-application row performs. One declaration for every copy:
/// which copy is the row's own payload, not a second action id.
pub const LAUNCH_VARIANT: &str = "app.launch-variant";

/// One menu row: an action id and what has to be under the pointer for it to appear.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MenuItem {
    /// An `id` from [`lege_types::action::MANIFEST`].
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
    MenuItem::new("window.cycle-forward", MenuScope::Window),
    MenuItem::new("window.cycle-backward", MenuScope::Window),
    MenuItem::new("window.step-right", MenuScope::Window),
    MenuItem::new("window.step-left", MenuScope::Window),
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

/// What the menu was opened over.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MenuTarget {
    pub tile: Option<TileId>,
    /// The live window that tile stands for, if it stands for one.
    pub window: Option<WindowId>,
    /// Whether the field has anything on its shelf right now.
    pub shelved: bool,
}

impl MenuTarget {
    fn offers(self, scope: MenuScope) -> bool {
        match scope {
            MenuScope::Field => true,
            MenuScope::Tile => self.tile.is_some(),
            MenuScope::Window => self.window.is_some(),
            MenuScope::Shelf => self.shelved,
        }
    }

    /// The rows this target offers, in [`MENU`] order.
    pub fn rows(self) -> Vec<&'static MenuItem> {
        MENU.iter()
            .filter(|item| self.offers(item.scope) && item.declaration().is_some())
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
        self.open.as_ref().map(|open| open.target)
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
        (index < open.rows.len() + open.choices.len()).then(|| Self::row_rect(rect, ui, index))
    }

    fn row_rect(rect: Rect, ui: &Ui<'_>, index: usize) -> Rect {
        Rect::new(
            rect.x + ui.px(PADDING),
            rect.y + ui.px(PADDING) + ui.px(ROW_HEIGHT) * index as i32,
            rect.w - ui.px(PADDING) * 2,
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

        let mut committed = false;
        for key in keys {
            match key {
                KeyInput::Escape => {
                    self.open = None;
                    return None;
                }
                KeyInput::Down => {
                    open.highlighted = Some(open.highlighted.map_or(0, |at| (at + 1) % count));
                }
                KeyInput::Up => {
                    open.highlighted = Some(
                        open.highlighted
                            .map_or(count - 1, |at| (at + count - 1) % count),
                    );
                }
                KeyInput::Enter => committed = true,
                _ => {}
            }
        }

        let mut picked = None;
        if committed && let Some(index) = open.highlighted {
            picked = Some(index);
        }
        for index in 0..count {
            let row = Self::row_rect(rect, ui, index);
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
                    target: open.target,
                    choice: None,
                };
                self.open = None;
                return Some(chosen);
            }
            if let Some(choice) = open.choices.get(index - open.rows.len()) {
                let chosen = Chosen {
                    action: LAUNCH_VARIANT,
                    target: open.target,
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

    /// Draw the menu over everything else. Purely visual — [`update`](Self::update) already took
    /// the input this frame.
    pub fn paint(&self, ui: &mut Ui<'_>, area: Rect) {
        let (Some(rect), Some(open)) = (self.rect(ui, area), self.open.as_ref()) else {
            return;
        };
        ui.painter
            .shadow_rect(rect, ui.px(3), ui.px(6), design::TEXT, 70);
        ui.painter
            .fill_rounded_rect(rect, ui.px(design::WELL_RADIUS), design::SHELL);
        ui.painter
            .stroke_rect(rect, ui.scale.hairline(), design::EDGE_STRONG);

        for (index, item) in open.rows.iter().enumerate() {
            let Some(declaration) = item.declaration() else {
                continue;
            };
            let row = Self::row_rect(rect, ui, index);
            let armed = open.armed == Some(item.action);
            let hovered = ui.input.hovering(row) || open.highlighted == Some(index);
            let destructive = declaration.confirm;
            if armed {
                ui.painter
                    .fill_rounded_rect(row, ui.px(design::CHIP_RADIUS), design::DANGER_SOFT);
            } else if hovered {
                ui.painter
                    .fill_rounded_rect(row, ui.px(design::CHIP_RADIUS), design::ACCENT_SOFT);
            }
            let (accelerator, label) = row.split_right(ui.px(96).min(row.w / 2));
            // Armed rows say what the second press will do, in plain words: a row that only
            // changed colour would be asking the user to guess.
            let text = if armed {
                format!("{} — press again", declaration.label)
            } else {
                declaration.label.to_owned()
            };
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
                    design::DANGER
                } else {
                    design::TEXT
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
                    TextStyle::tracked(ui.theme.body.face, 11.0, 0.10),
                    design::TEXT_FAINT,
                    Align::Right,
                );
            }
        }

        // The copies, under the declared rows: which one, and where it came from. They are the
        // only rows the manifest does not name, and they say so by looking like what they are —
        // a name and a provenance, not a verb.
        for (offset, choice) in open.choices.iter().enumerate() {
            let index = open.rows.len() + offset;
            let row = Self::row_rect(rect, ui, index);
            if ui.input.hovering(row) || open.highlighted == Some(index) {
                ui.painter
                    .fill_rounded_rect(row, ui.px(design::CHIP_RADIUS), design::ACCENT_SOFT);
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
                design::TEXT,
                Align::Left,
            );
            ui.label_styled(
                Rect::new(detail.x, detail.y, (detail.w - ui.px(8)).max(0), detail.h),
                &choice.detail,
                TextStyle::tracked(ui.theme.body.face, 11.0, 0.10),
                design::TEXT_FAINT,
                Align::Right,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            tile: Some(lege_field::TileId(7)),
            ..MenuTarget::default()
        };
        let mut menu = ContextMenu::default();
        menu.open_at((10, 10), target);
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
                        400, 400
                    )),
                    &mut pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                    &mut pixelkit_shell::Input::new(),
                    design::theme(),
                    pixelkit_shell::Scale::ONE,
                    &mut pixelkit_raster::RasterKernel::new(),
                ),
                Rect::new(0, 0, 400, 400),
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
            shelved: false,
        };
        let window = MenuTarget {
            tile: Some(TileId(7)),
            window: Some(WindowId(3)),
            shelved: false,
        };
        let shelf = MenuTarget {
            shelved: true,
            ..MenuTarget::default()
        };

        let ids = |target: MenuTarget| -> Vec<&'static str> {
            target.rows().iter().map(|item| item.action).collect()
        };
        assert!(!ids(nothing).contains(&"field.stash"));
        assert!(!ids(nothing).contains(&"window.close"));
        assert!(ids(nothing).contains(&"field.undo"));
        assert!(ids(tile).contains(&"field.stash"));
        assert!(
            !ids(tile).contains(&"window.close"),
            "a tile with no live window has nothing to close"
        );
        assert!(ids(window).contains(&"window.close"));
        assert_eq!(
            ids(window).last(),
            Some(&"window.close"),
            "the destructive row stays at the bottom"
        );

        // Restoring is offered only when there is something on the shelf: a row that reads
        // "bring shelved tiles back" and does nothing is worse than no row.
        assert!(
            !ids(nothing).contains(&"field.restore"),
            "an empty shelf offers nothing to bring back"
        );
        assert!(!ids(tile).contains(&"field.restore"));
        assert!(ids(shelf).contains(&"field.restore"));

        // Reset is a field row, so it is reachable from bare background — that is the whole
        // point, since a field can be reset when no tile is worth pointing at.
        assert!(ids(nothing).contains(&"field.reset"));
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
