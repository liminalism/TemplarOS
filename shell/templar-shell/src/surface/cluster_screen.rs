//! The machine cluster: Settings, the network, the clock, and the account — at the bottom-right
//! corner of the screen, on a surface of its own.
//!
//! It used to be the right-hand end of the bar. That became wrong twice over when the bar turned
//! into a centred island of open windows: what the machine is doing is not what the switcher is
//! for, and centring it put a clock and a login menu in the middle of the screen. So it is its own
//! surface, anchored to the *end* of the bottom edge (`templar-shell-v1.xml`'s `set_edge_alignment`),
//! and the island is open windows and nothing else.
//!
//! **It is absent in fullscreen**, entirely — no hint, no peek, no surface. A fullscreen window is
//! one thing being looked at and a battery percentage is not part of it. The switcher still comes
//! back on a dwell, because moving between windows is the one thing a person still needs there.
//!
//! Like the island it reserves nothing, and its transparent parts are desktop: the coverage it
//! records ([`ClusterScreen::opaque_shapes`]) is the chips themselves, and the host turns that
//! into both the surface's alpha and its input region.

use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use crate::surface::bar_screen::{BAR_HEIGHT, BarAction, CHIP_GAP, CHIP_H, CHIP_PAD, chip};
use crate::surface::design::{self, Elevation};
use crate::surface::field_screen::FieldAction;

/// The Settings chip — the whole of Settings, not one corner of it. It took the readings chip's
/// place in the cluster.
const SETTINGS_WIDTH: i32 = 112;
const NETWORK_WIDTH: i32 = 96;
const CLOCK_WIDTH: i32 = 160;
/// The account chip: the account's name, its mark, and the popover behind it.
const ACCOUNT_WIDTH: i32 = 140;
/// The account popover's width and one row's height, and the padding inside its frame.
const POPOVER_WIDTH: i32 = 232;
const POPOVER_ROW: i32 = 28;
const POPOVER_PAD: i32 = 8;
/// Grace after the pointer leaves the cluster before an open popover closes. The same number the
/// island's hover row uses, and for the same reason: long enough to cross a gap, short enough that
/// a menu never outstays the pointer.
pub const POPOVER_CLOSE_MS: u64 = 200;

/// The gap between the popover's frame and the bar it grows out of.
const POPOVER_GAP: i32 = 6;

/// The account popover's rows, in order.
///
/// Written out as literal `RunAction` constructions rather than as a table of bare ids on
/// purpose: that spelling is what `xtask check-actions` reads as a pointer route, and this popover
/// is now the only pointer route to the session and machine actions — Home no longer carries the
/// Session or Settings widget.
///
/// `settings.open` is not here any more: it has its own chip in the cluster, and an action offered
/// in two places is one a person has to look for twice.
const ACCOUNT_ROWS: [FieldAction; 10] = [
    FieldAction::RunAction("volume.decrease"),
    FieldAction::RunAction("volume.mute"),
    FieldAction::RunAction("volume.increase"),
    FieldAction::RunAction("brightness.decrease"),
    FieldAction::RunAction("brightness.increase"),
    FieldAction::RunAction("session.lock"),
    FieldAction::RunAction("session.suspend"),
    FieldAction::RunAction("session.logout"),
    FieldAction::RunAction("session.reboot"),
    FieldAction::RunAction("session.poweroff"),
];

/// The action id a popover row performs. The table above is built entirely of `RunAction`s.
fn row_id(row: &FieldAction) -> &'static str {
    match row {
        FieldAction::RunAction(id) => id,
        _ => "",
    }
}

/// How far above the bar the account popover reaches, gap included. The readings line is the
/// first row and costs a row's height like any other.
pub fn popover_band() -> i32 {
    POPOVER_GAP + 2 * POPOVER_PAD + (ACCOUNT_ROWS.len() as i32 + 1) * POPOVER_ROW
}
/// The popover's first line: charge, volume and backlight, as the services last reported them.
/// A reading with no hardware behind it is left out rather than shown as a dash — a machine with
/// no backlight has nothing to say about brightness.
fn readings_line(machine: &crate::data::MachineReadings) -> String {
    let mut parts = vec![machine.chip_label()];
    if let Some(audio) = machine.audio.as_ref().filter(|audio| audio.present) {
        parts.push(format!("Volume {}%", audio.volume_percent));
    }
    if let Some(backlight) = machine.backlight.as_ref().filter(|light| light.present) {
        parts.push(format!("Brightness {}%", backlight.percent));
    }
    parts.join(" \u{b7} ")
}

/// What the cluster draws this frame.
#[derive(Debug)]
pub struct ClusterData<'a> {
    pub clock_label: &'a str,
    /// Whether the network control's role has a provider that can be launched.
    pub network_control_available: bool,
    /// Whether an installed application declares `Categories=Calendar`, so the clock has
    /// somewhere to open into.
    pub calendar_available: bool,
    /// Who is signed in: what the account chip says next to its mark.
    pub account_label: &'a str,
    /// Charge, volume and backlight, as the services last reported them.
    pub machine: &'a crate::data::MachineReadings,
    pub now_ms: u64,
}

/// Where each chip and the popover are, in the surface's own coordinates.
#[derive(Debug)]
struct Layout {
    settings: Rect,
    network: Rect,
    clock: Rect,
    account: Rect,
    /// The account popover's frame and its rows, when it is open and the host has made room.
    popover: Option<(Rect, Vec<Rect>)>,
}

#[derive(Debug, Default)]
pub struct ClusterScreen {
    /// Whether the account popover is open.
    pub account_open: bool,
    /// The popover row armed by a first click and waiting for its second.
    ///
    /// `ActionDecl::confirm` is set on the three actions that end the session, and a chip in the
    /// corner of the screen is exactly the sort of thing a sleeve catches: without this, brushing
    /// the popover would shut the machine down.
    confirming: Option<&'static str>,
    /// The shapes this frame painted solid, for the host's alpha and input regions.
    opaque: Vec<(Rect, i32)>,
    /// When the pointer left the cluster's surface, while the popover is open.
    left_at: Option<u64>,
}

/// How wide the cluster is: its two margins, its four chips, and the gaps between them.
#[must_use]
pub fn cluster_width() -> i32 {
    2 * CHIP_PAD + SETTINGS_WIDTH + NETWORK_WIDTH + CLOCK_WIDTH + ACCOUNT_WIDTH + 3 * CHIP_GAP
}

/// The surface size the host should ask for. The popover grows it upward, exactly as the island's
/// hover row does, and reserves nothing either way.
#[must_use]
pub fn wanted_size(account_open: bool, full_width: i32) -> (i32, i32) {
    let band = if account_open { popover_band() } else { 0 };
    (cluster_width().min(full_width.max(1)), BAR_HEIGHT + band)
}

impl ClusterScreen {
    /// The shapes this frame painted solid. Empty before the first draw, which the host reads as
    /// "no coverage yet" and answers with an opaque present.
    #[must_use]
    pub fn opaque_shapes(&self) -> &[(Rect, i32)] {
        &self.opaque
    }

    /// Whether the host should make room above the cluster for the account popover.
    #[must_use]
    pub fn account_open(&self) -> bool {
        self.account_open
    }

    fn layout(ui: &Ui<'_>, area: Rect, account_open: bool) -> Layout {
        let bar_height = ui.px(BAR_HEIGHT).min(area.h);
        let bar = Rect::new(area.x, area.bottom() - bar_height, area.w, bar_height);
        let chip_h = ui.px(CHIP_H).min(bar.h);
        let chip_y = bar.y + (bar.h - chip_h) / 2;
        let gap = ui.px(CHIP_GAP);

        // Right to left, because the right-hand end of the screen is what this is anchored to.
        let mut right = bar.right() - ui.px(CHIP_PAD);
        let mut place = |width: i32| {
            let rect = Rect::new(right - width, chip_y, width, chip_h);
            right = rect.x - gap;
            rect
        };
        let account = place(ui.px(ACCOUNT_WIDTH));
        let clock = place(ui.px(CLOCK_WIDTH));
        let network = place(ui.px(NETWORK_WIDTH));
        let settings = place(ui.px(SETTINGS_WIDTH));

        let popover = (account_open && area.h > bar_height).then(|| {
            let width = ui.px(POPOVER_WIDTH).min(area.w);
            let row_h = ui.px(POPOVER_ROW);
            let pad = ui.px(POPOVER_PAD);
            let height =
                (2 * pad + (ACCOUNT_ROWS.len() as i32 + 1) * row_h).min(area.h - bar_height);
            let frame = Rect::new(
                (account.right() - width).max(area.x),
                bar.y - ui.px(POPOVER_GAP) - height,
                width,
                height,
            );
            let rows = (0..=ACCOUNT_ROWS.len() as i32)
                .map(|i| {
                    Rect::new(
                        frame.x + pad,
                        frame.y + pad + i * row_h,
                        (frame.w - 2 * pad).max(0),
                        row_h,
                    )
                })
                .collect();
            (frame, rows)
        });

        Layout {
            settings,
            network,
            clock,
            account,
            popover,
        }
    }

    /// Paint the cluster and answer this frame's clicks.
    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &ClusterData<'_>) -> Vec<BarAction> {
        let mut actions = Vec::new();
        let layout = Self::layout(ui, area, self.account_open);

        // The ground first, for the opaque fallback; the chips are the coverage, so everything
        // between and around them is desktop. The same bargain the island makes.
        ui.painter.fill_rect(area, design::ground());
        let radius = ui.px(design::CHIP_RADIUS);
        self.opaque.clear();
        for shape in [
            layout.settings,
            layout.network,
            layout.clock,
            layout.account,
        ] {
            self.opaque.push((shape, radius));
        }

        // The account popover first, for the same reason the row is early: it floats over what is
        // above the bar and must claim its clicks before anything underneath can.
        if let Some((frame, rows)) = &layout.popover {
            // `design::shell` fills at TILE_RADIUS, not CHIP_RADIUS. Recording the smaller one
            // leaves the coverage solid where the paint has already curved away — four dark ears
            // on every corner, of the shadow behind it.
            self.opaque.push((*frame, ui.px(design::TILE_RADIUS)));
            design::shell(
                ui,
                *frame,
                design::shell_fill(),
                design::edge_strong(),
                Elevation::Lifted,
            );
            // The readings first, as a line rather than a row: what the machine is doing belongs
            // beside the controls that change it, and it is the one line here that is not a
            // button. It replaces the chip the cluster used to spend 112 pixels on.
            if let Some(first) = rows.first() {
                ui.label_styled(
                    first.inset(ui.px(6)),
                    &readings_line(data.machine),
                    design::state_style(ui),
                    design::text_faint(),
                    Align::Left,
                );
            }
            for (rect, row) in rows.iter().skip(1).zip(ACCOUNT_ROWS) {
                let id = row_id(&row);
                let Some(declaration) = templar_types::action::action(id) else {
                    continue;
                };
                let refused = data.machine.refuses(id);
                let armed = self.confirming == Some(id);
                let hovered = !refused && ui.input.hovering(*rect);
                if armed || hovered {
                    ui.painter.fill_rounded_rect(
                        *rect,
                        ui.px(design::CHIP_RADIUS),
                        if armed {
                            design::danger()
                        } else {
                            design::accent_soft()
                        },
                    );
                }
                // A row that ends the session says so twice: an ellipsis before it is armed,
                // and then in words. "Click again to log out" is a sentence; "Log out — again to
                // confirm" is a state machine talking to itself.
                let label = if armed {
                    format!("Click again to {}", declaration.label.to_lowercase())
                } else if declaration.confirm {
                    format!("{}\u{2026}", declaration.label)
                } else {
                    declaration.label.to_owned()
                };
                ui.label_styled(
                    rect.inset(ui.px(6)),
                    &label,
                    design::state_style(ui),
                    if refused {
                        design::text_faint()
                    } else {
                        design::text()
                    },
                    Align::Left,
                );
                if !refused && ui.input.take_click(*rect) {
                    // Two steps for the three actions that end the session, one for everything
                    // else. A different row cancels whatever was armed.
                    if declaration.confirm && !armed {
                        self.confirming = Some(id);
                    } else {
                        self.confirming = None;
                        self.account_open = false;
                        actions.push(BarAction::Field(FieldAction::RunAction(id)));
                    }
                }
            }
            // Anything else inside the frame is the popover's own ground, not the window behind.
            let _ = ui.input.take_click(*frame);
        }

        // The machine cluster, left to right: Settings, the network, the clock, the account.
        // The label comes from the action manifest for the same reason the network's does.
        let settings_label = templar_types::action::action("settings.open")
            .map_or("Settings", |declaration| declaration.label);
        let settings_hovered = ui.input.hovering(layout.settings);
        chip(
            ui,
            layout.settings,
            if settings_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.settings.inset(ui.px(6)),
            &format!("\u{2699} {settings_label}"),
            design::state_style(ui),
            design::TAG_INK,
            Align::Centre,
        );
        if ui.input.take_click(layout.settings) {
            actions.push(BarAction::Field(FieldAction::RunAction("settings.open")));
        }

        // The label comes from the action manifest, so the bar and the cheat sheet
        // cannot call the same capability two different things.
        let network_label = templar_types::action::action("network.open-control")
            .map_or("Network", |declaration| declaration.label);
        let network_hovered = data.network_control_available && ui.input.hovering(layout.network);
        chip(
            ui,
            layout.network,
            if network_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.network.inset(ui.px(6)),
            network_label,
            design::state_style(ui),
            if data.network_control_available {
                design::TAG_INK
            } else {
                design::text_faint()
            },
            Align::Centre,
        );
        if data.network_control_available && ui.input.take_click(layout.network) {
            actions.push(BarAction::Field(FieldAction::RunAction(
                "network.open-control",
            )));
        }

        // The clock opens the installed calendar. Dimmed when none is installed, the way the
        // network chip is: a chip that looks live and does nothing is worse than one that says so.
        let clock_hovered = data.calendar_available && ui.input.hovering(layout.clock);
        chip(
            ui,
            layout.clock,
            if clock_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.clock.inset(ui.px(6)),
            data.clock_label,
            design::state_style(ui),
            if data.calendar_available {
                design::TAG_INK
            } else {
                design::text_faint()
            },
            Align::Centre,
        );
        if data.calendar_available && ui.input.take_click(layout.clock) {
            actions.push(BarAction::Field(FieldAction::RunAction(
                "clock.open-calendar",
            )));
        }

        // The account chip: Settings, the machine's controls and the session actions, in one
        // place, because there is one of each of them on this desktop.
        let account_hovered = ui.input.hovering(layout.account);
        chip(
            ui,
            layout.account,
            if self.account_open || account_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.account.inset(ui.px(6)),
            &format!("\u{25c9} {}", data.account_label),
            design::state_style(ui),
            design::TAG_INK,
            Align::Centre,
        );
        if ui.input.take_click(layout.account) {
            self.account_open = !self.account_open;
            self.confirming = None;
        }

        // Leaving closes it. A press on the surface's own ground cannot: the input region is the
        // chips and the popover, so the ground is desktop and its clicks go to whatever is behind.
        // That left the popover with no way out but the chip that opened it — a menu that stays up
        // while you use the window underneath. The row's rule instead: the pointer leaves, a grace
        // period passes, it goes.
        if self.account_open {
            if ui.input.cursor().is_some_and(|(x, y)| area.contains(x, y)) {
                self.left_at = None;
            } else {
                let left = *self.left_at.get_or_insert(data.now_ms);
                if data.now_ms.saturating_sub(left) >= POPOVER_CLOSE_MS {
                    self.account_open = false;
                    self.confirming = None;
                    self.left_at = None;
                }
            }
        } else {
            self.left_at = None;
        }
        actions
    }

    /// When the host should wake to close a popover the pointer has left, if it should.
    #[must_use]
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.left_at.map(|left| left + POPOVER_CLOSE_MS)
    }
}
