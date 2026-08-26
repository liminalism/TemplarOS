//! The widget contract: what a `TileKind::System` tile is, and how it draws.
//!
//! Everything the Home Field lacks — a launcher, session power, volume, brightness, battery,
//! notifications — is meant to arrive as a widget on this one screen rather than as a panel, a
//! tray or a menu bar (`docs/generated/ROADMAP.md` M2: taskbar-free, "rather than a permanent
//! dock"). A widget **shows information first**; its regions can be clicked; opening a window is
//! a separate, deliberate second step.
//!
//! Three rules make that more than a slogan:
//!
//! 1. **The shell renders; it never decides.** A widget's content is a [`WidgetSnapshot`] handed
//!    in from a service (`legeos.constraint.services-not-indicators`). Nothing here talks to
//!    D-Bus, so every widget is testable against a plain buffer.
//! 2. **A refresh must never move a tile.** Snapshots change content only, which by the field's
//!    own rule moves nothing (`docs/homefield.md`, "Do not let reflow destroy memory"; the
//!    `content_changes_move_nothing` property).
//! 3. **Regions are declared, not coded.** A [`WidgetRegion`] names an action from
//!    [`lege_types::action::MANIFEST`], so the same declaration that gives an action its label,
//!    its accelerator and its `lege` verb is what puts it on a tile. There is no per-widget
//!    `match` arm in the painter, and no way to add a clickable thing that the cheat sheet and
//!    the CLI do not know about.
//!
//! Sizing follows the representation ladder: a region declares the smallest
//! [`Representation`] at which it appears, so a tile that shrinks *drops* controls rather than
//! rendering them too small to hit.

pub mod launcher;

use lege_field::Representation;
use lege_types::action::{self, ActionDecl};
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use crate::surface::design;

/// What a widget is currently showing. Produced by a service-backed fetch, never by the view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WidgetSnapshot {
    /// The one figure or state worth reading at a glance. Shown at every rung.
    pub headline: String,
    /// Supporting lines, shown from [`Representation::Wide`] upward.
    pub detail: Vec<String>,
    /// Whether the backing service answered. A widget that cannot reach its service says so
    /// rather than showing a plausible zero — the same honesty the Network drawer already
    /// practises (`crate::drawers::network`).
    pub available: bool,
}

impl WidgetSnapshot {
    /// A snapshot for a service that is not reachable.
    pub fn unavailable(reason: impl Into<String>) -> WidgetSnapshot {
        WidgetSnapshot {
            headline: reason.into(),
            detail: Vec::new(),
            available: false,
        }
    }
}

/// One clickable region on a widget, naming an action rather than describing a behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetRegion {
    /// An `id` from [`lege_types::action::MANIFEST`].
    pub action: &'static str,
    /// The smallest rung at which this region is offered. Below it the region is not drawn and
    /// not hit-testable — never drawn-but-too-small, which is how a control becomes a trap.
    pub from_rung: Representation,
}

impl WidgetRegion {
    pub const fn new(action: &'static str, from_rung: Representation) -> WidgetRegion {
        WidgetRegion { action, from_rung }
    }

    /// The declaration this region points at.
    pub fn declaration(&self) -> Option<&'static ActionDecl> {
        action::action(self.action)
    }
}

/// One widget kind. `id` is what a tile's `TileKind::System { probe }` carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetDescriptor {
    pub id: &'static str,
    /// The tile's heading.
    pub title: &'static str,
    pub regions: &'static [WidgetRegion],
}

/// Every widget the shell can draw.
///
/// Session is first because it is the one a desktop is unusable without: before it there was no
/// route from a running session to a logged-out one that did not involve killing the compositor
/// from a virtual terminal. Launcher is the second for the mirror-image reason: application tiles
/// are derived from windows that already exist, so a freshly booted session had nothing on it to
/// open anything *with*.
pub const WIDGETS: &[WidgetDescriptor] = &[
    WidgetDescriptor {
        id: "session",
        title: "Session",
        regions: &[
            WidgetRegion::new("session.lock", Representation::Small),
            WidgetRegion::new("session.suspend", Representation::Wide),
            WidgetRegion::new("session.logout", Representation::Wide),
            WidgetRegion::new("session.reboot", Representation::Large),
            WidgetRegion::new("session.poweroff", Representation::Small),
        ],
    },
    WidgetDescriptor {
        id: "launcher",
        title: "Applications",
        // The rows themselves are the launcher's real surface (`launcher::draw`), and each one
        // performs `launcher.launch`; these two are the controls that are the same whatever is
        // listed. `launcher.show-all` only earns its place once there is room for enough rows for
        // "all" to differ visibly from what a filter left behind.
        regions: &[
            WidgetRegion::new("launcher.search", Representation::Small),
            WidgetRegion::new("launcher.show-all", Representation::Wide),
        ],
    },
];

/// The widget `id` names, if the shell knows it.
pub fn descriptor(id: &str) -> Option<&'static WidgetDescriptor> {
    WIDGETS.iter().find(|widget| widget.id == id)
}

/// The regions offered at `rung`, in declaration order.
pub fn regions_at(
    widget: &'static WidgetDescriptor,
    rung: Representation,
) -> Vec<&'static WidgetRegion> {
    widget
        .regions
        .iter()
        .filter(|region| rung >= region.from_rung)
        .collect()
}

/// The strip of a widget's content area given over to its regions, and the body above it.
///
/// A fixed proportion rather than a fixed height, so the controls stay in the same relative
/// place as a tile grows — the spatial-memory rule applied inside one tile.
fn split_regions(content: Rect, ui: &Ui<'_>, count: usize) -> (Rect, Option<Rect>) {
    if count == 0 {
        return (content, None);
    }
    let strip = ui.px(30).min(content.h / 2).max(0);
    if strip <= 0 {
        return (content, None);
    }
    let body = Rect::new(content.x, content.y, content.w, content.h - strip);
    let regions = Rect::new(content.x, content.bottom() - strip, content.w, strip);
    (body, Some(regions))
}

/// The rectangle of region `index` of `count`, laid out as an even row.
fn region_rect(strip: Rect, index: usize, count: usize) -> Rect {
    let count = count.max(1) as i32;
    let index = index as i32;
    let width = strip.w / count;
    let x = strip.x + width * index;
    // The last region takes the rounding remainder, so the row always fills the strip exactly.
    let width = if index + 1 == count {
        strip.right() - x
    } else {
        width
    };
    Rect::new(x, strip.y, width, strip.h)
}

/// Draw one widget into `content` and report the action a click asked for.
///
/// `confirming` is the action currently awaiting its second press, if any: a destructive
/// action ([`ActionDecl::confirm`]) never fires on one click, so the first press arms it and
/// only the second returns it. The caller owns that state because it must survive across frames
/// and be cleared when the pointer goes elsewhere.
pub fn draw(
    ui: &mut Ui<'_>,
    content: Rect,
    widget: &'static WidgetDescriptor,
    snapshot: Option<&WidgetSnapshot>,
    rung: Representation,
    confirming: Option<&'static str>,
) -> Option<&'static ActionDecl> {
    let (body, chosen) = draw_region_strip(ui, content, widget, rung, confirming);
    draw_snapshot(ui, body, snapshot, rung);
    chosen
}

/// Lay out and draw a widget's declared region row along the bottom of `content`, returning the
/// body above it and the action a click asked for.
///
/// Split out of [`draw`] so a widget whose body is something other than a headline and a few
/// lines — the launcher's list of applications, say — still gets its controls in the same place,
/// at the same size, obeying the same rung rules, rather than inventing its own row.
pub fn draw_region_strip(
    ui: &mut Ui<'_>,
    content: Rect,
    widget: &'static WidgetDescriptor,
    rung: Representation,
    confirming: Option<&'static str>,
) -> (Rect, Option<&'static ActionDecl>) {
    let offered = regions_at(widget, rung);
    let (body, strip) = split_regions(content, ui, offered.len());
    let Some(strip) = strip else {
        return (body, None);
    };
    let mut chosen = None;
    for (index, region) in offered.iter().enumerate() {
        let Some(declaration) = region.declaration() else {
            continue;
        };
        let rect = region_rect(strip, index, offered.len()).inset(ui.px(3));
        if rect.w <= 0 || rect.h <= 0 {
            continue;
        }
        let armed = confirming == Some(declaration.id);
        let hovered = ui.input.hovering(rect);
        let (fill, ink) = match (armed, declaration.confirm) {
            (true, _) => (design::DANGER, 0x00ff_ffff),
            (false, true) if hovered => (design::DANGER_SOFT, design::DANGER),
            (false, _) if hovered => (design::ACCENT_SOFT, design::ACCENT),
            (false, true) => (design::SHELL_LOW, design::DANGER),
            (false, false) => (design::SHELL_LOW, design::TEXT),
        };
        ui.painter
            .fill_rounded_rect(rect, ui.px(design::CHIP_RADIUS), fill);
        let label = if armed { "CONFIRM" } else { declaration.label };
        ui.label_styled(rect, label, design::state_style(ui), ink, Align::Centre);
        if ui.input.take_click(rect) {
            chosen = Some(declaration);
        }
    }
    (body, chosen)
}

/// The default widget body: one headline, and supporting lines from [`Representation::Wide`] up.
fn draw_snapshot(
    ui: &mut Ui<'_>,
    body: Rect,
    snapshot: Option<&WidgetSnapshot>,
    rung: Representation,
) {
    let inner = body.inset(ui.px(10));
    if inner.h > 0 {
        match snapshot {
            None => ui.label(inner, "…", design::TEXT_FAINT, Align::Left),
            Some(snapshot) => {
                let colour = if snapshot.available {
                    design::TEXT
                } else {
                    design::TEXT_DIM
                };
                ui.label_styled(
                    inner,
                    &snapshot.headline,
                    design::measurement_style(ui),
                    colour,
                    Align::Left,
                );
                if rung >= Representation::Wide {
                    let line_height = ui.px(16);
                    for (index, line) in snapshot.detail.iter().enumerate() {
                        let y = inner.y + line_height * (index as i32 + 1) + ui.px(6);
                        if y + line_height > inner.bottom() {
                            break;
                        }
                        ui.label_styled(
                            Rect::new(inner.x, y, inner.w, line_height),
                            line,
                            design::state_style(ui),
                            design::TEXT_DIM,
                            Align::Left,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_region_names_a_real_action() {
        for widget in WIDGETS {
            for region in widget.regions {
                assert!(
                    region.declaration().is_some(),
                    "{} declares region {:?}, which is in no action manifest",
                    widget.id,
                    region.action
                );
            }
        }
    }

    #[test]
    fn every_widget_id_is_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for widget in WIDGETS {
            assert!(seen.insert(widget.id), "duplicate widget id {}", widget.id);
        }
    }

    #[test]
    fn a_shrinking_widget_drops_controls_rather_than_squeezing_them() {
        let session = descriptor("session").expect("the session widget exists");
        let small = regions_at(session, Representation::Small).len();
        let wide = regions_at(session, Representation::Wide).len();
        let large = regions_at(session, Representation::Large).len();
        assert!(small < wide && wide < large, "{small} {wide} {large}");
        assert_eq!(
            large,
            session.regions.len(),
            "the largest rung offers everything"
        );
        // Whatever else is dropped, the two a person reaches for must survive to the floor.
        let names: Vec<&str> = regions_at(session, Representation::Small)
            .iter()
            .map(|region| region.action)
            .collect();
        assert!(names.contains(&"session.lock"));
        assert!(names.contains(&"session.poweroff"));
    }

    #[test]
    fn the_region_row_fills_its_strip_exactly_and_never_overlaps() {
        let strip = Rect::new(10, 100, 97, 30);
        for count in 1..=5usize {
            let rects: Vec<Rect> = (0..count).map(|i| region_rect(strip, i, count)).collect();
            assert_eq!(rects[0].x, strip.x);
            assert_eq!(
                rects[count - 1].right(),
                strip.right(),
                "count {count} left a gap at the end"
            );
            for pair in rects.windows(2) {
                assert_eq!(pair[0].right(), pair[1].x, "count {count} overlaps or gaps");
            }
        }
    }

    #[test]
    fn an_unreachable_service_says_so_instead_of_showing_a_plausible_zero() {
        let snapshot = WidgetSnapshot::unavailable("Session — no service");
        assert!(!snapshot.available);
        assert!(snapshot.headline.contains("no service"));
    }
}
