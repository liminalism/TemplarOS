//! The widget contract: what a `TileKind::System` tile is, and how it draws.
//!
//! Everything the Home Field lacks — a launcher, session power, notifications, the hardware
//! readings — is meant to arrive as a widget on this one screen rather than as a panel, a
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
//!    own rule moves nothing (`@legeos.decision.home-field-stable-bounded-semantic-grammar`; the
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

pub mod notifications;
pub mod settings;

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
    /// Action ids the service has said it cannot perform right now — logind refusing a shutdown
    /// by policy, say. Their regions are drawn dimmed and do not take a click.
    ///
    /// The service already knew this and the widget already displayed it *in prose*, in a detail
    /// line no smaller rung even shows. A button that is known to be refused and is nevertheless
    /// drawn exactly like a working one is a promise the shell cannot keep, so the knowledge has
    /// to reach the region and not just the paragraph beside it.
    pub refused: Vec<String>,
}

impl WidgetSnapshot {
    /// A snapshot for a service that is not reachable.
    pub fn unavailable(reason: impl Into<String>) -> WidgetSnapshot {
        WidgetSnapshot {
            headline: reason.into(),
            detail: Vec::new(),
            available: false,
            refused: Vec::new(),
        }
    }

    /// Whether `action` is one the service has said it cannot perform.
    pub fn refuses(&self, action: &str) -> bool {
        self.refused.iter().any(|refused| refused == action)
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
    /// What a wheel over this widget's body means, as `(scroll up, scroll down)` action ids.
    ///
    /// A level is the one kind of widget a wheel is genuinely the right gesture for, and a person
    /// who has used any other desktop will try it. It stays inside rule 3 all the same: these are
    /// ids from the same manifest the regions name, so the wheel is another *route* to a declared
    /// action — as a chord is — and never a behaviour that exists only here. `None` is the normal
    /// case, and a widget with nothing to step must not silently eat the wheel.
    pub scroll: Option<(&'static str, &'static str)>,
    /// The span this widget wants when it is placed, as `(cols, rows)`.
    ///
    /// Per widget rather than one figure for every `TileKind::System`, because the class default
    /// is the launcher's — a list needs rows to be a list — and not every widget is a list. With
    /// several widgets sharing one shelf the difference is the whole desktop: at the class's own
    /// 4x2 they take rows of a seven-row field that are furniture standing where the windows are
    /// meant to go. Every system tile keeps the same 2x1 floor it always had, so this changes what
    /// a widget *arrives* at and never what a person may shrink it to.
    pub preferred_span: (u16, u16),
}

/// Every widget the shell can draw.
///
/// Session is first because it is the one a desktop is unusable without: before it there was no
/// route from a running session to a logged-out one that did not involve killing the compositor
/// from a virtual terminal. Notifications and Settings follow; launching is the search control,
/// not a fourth tile.
pub const WIDGETS: &[WidgetDescriptor] = &[
    WidgetDescriptor {
        id: "session",
        title: "Session",
        // Which two survive to the floor is the whole design of this tile, and the first attempt
        // had it backwards: Lock and *Shut down*. A session tile is created at its floor and stays
        // there until someone resizes it, so on every real desktop that meant the destructive
        // action was the one that could never be hidden, while Log out — the thing a person
        // actually reaches for several times a day — was behind a resize that the default field
        // density makes inert (`@legeos.question.home-field-default-density`). Lock and Log out
        // are the two that survive; ending the machine is a rung further in, and confirmed
        // (`ActionDecl::confirm`) on top of that.
        regions: &[
            WidgetRegion::new("session.lock", Representation::Small),
            WidgetRegion::new("session.logout", Representation::Small),
            WidgetRegion::new("session.suspend", Representation::Wide),
            WidgetRegion::new("session.reboot", Representation::Large),
            WidgetRegion::new("session.poweroff", Representation::Large),
        ],
        scroll: None,
        preferred_span: (3, 2),
    },
    WidgetDescriptor {
        id: "notifications",
        title: "Notifications",
        regions: &[WidgetRegion::new(
            "notifications.clear",
            Representation::Small,
        )],
        scroll: None,
        preferred_span: (3, 2),
    },
    // What the hardware is doing, and the one way in to changing it. This was three tiles —
    // Battery, Volume, Brightness — and three tiles is what a tray looks like: three-fifths of the
    // system shelf spent on three numbers, each with its own heading repeating a word the figure
    // beside it already implied. One tile carries all three readings on one line each and opens
    // into a window with room to explain the rest (`settings.open`). Nothing here decides
    // anything: the readings are handed in (`legeos.constraint.services-not-indicators`).
    WidgetDescriptor {
        id: "settings",
        title: "Settings",
        // The ladder is the point of merging them. At the floor the tile is a readout with one way
        // in, which is what a glance needs. Grown a rung it becomes the quick controls the three
        // separate tiles used to offer, and a rung above that the one control with a key of its own
        // on most keyboards. Stepping is never *only* available up there: the wheel below works at
        // every rung, so the floor still changes the volume in one gesture.
        regions: &[
            WidgetRegion::new("settings.open", Representation::Small),
            WidgetRegion::new("volume.decrease", Representation::Wide),
            WidgetRegion::new("volume.increase", Representation::Wide),
            WidgetRegion::new("brightness.decrease", Representation::Wide),
            WidgetRegion::new("brightness.increase", Representation::Wide),
            WidgetRegion::new("volume.mute", Representation::Large),
        ],
        // Volume, not brightness: a wheel has one meaning per widget, and of the two levels this
        // tile reports it is the one a person reaches for without looking.
        scroll: Some(("volume.increase", "volume.decrease")),
        // Three lines and a control row. Smaller than the launcher's list still, and the same six
        // cells the three tiles it replaces used to claim between them at their smallest.
        preferred_span: (3, 2),
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
/// The region strip's height in logical pixels, before it is clamped to half the content.
const REGION_STRIP_HEIGHT: i32 = 30;

fn split_regions(content: Rect, ui: &Ui<'_>, count: usize) -> (Rect, Option<Rect>) {
    if count == 0 {
        return (content, None);
    }
    let strip = ui.px(REGION_STRIP_HEIGHT).min(content.h / 2).max(0);
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
    // No snapshot yet means nothing is *known* to be refused — the controls stay live rather than
    // greying out while the first fetch is still in flight.
    let refused = snapshot.map_or(&[][..], |snapshot| &snapshot.refused);
    let (body, chosen) = draw_region_strip(ui, content, widget, rung, confirming, refused);
    draw_snapshot(ui, body, snapshot, rung);
    // A click on a region wins over a wheel in the same frame: the click is the deliberate one.
    chosen.or_else(|| scrolled_action(ui, body, widget, refused))
}

/// The action a wheel over `body` asked for, if this widget declares one.
///
/// Taken from the body rather than the whole tile so the wheel cannot fight the region strip, and
/// `take_scroll` claims it, so a wheel over a widget does not also scroll whatever is underneath.
fn scrolled_action(
    ui: &mut Ui<'_>,
    body: Rect,
    widget: &'static WidgetDescriptor,
    refused: &[String],
) -> Option<&'static ActionDecl> {
    let (up, down) = widget.scroll?;
    let (_, dy) = ui.input.take_scroll(body);
    if dy == 0.0 {
        return None;
    }
    // Wheel up is a negative delta, and wheel up means more.
    let id = if dy < 0.0 { up } else { down };
    if refused.iter().any(|entry| entry == id) {
        return None;
    }
    action::action(id)
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
    // Action ids the backing service has said it cannot perform (`WidgetSnapshot::refused`).
    refused: &[String],
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
        // A refused region still appears — removing it would leave a person wondering where the
        // control went — but it is visibly inert and does not respond to hover or to a click.
        let refused = refused.iter().any(|id| id == declaration.id);
        let (fill, ink) = match (refused, armed, declaration.confirm) {
            (true, _, _) => (design::SHELL_LOW, design::TEXT_DIM),
            (false, true, _) => (design::DANGER, 0x00ff_ffff),
            (false, false, true) if hovered => (design::DANGER_SOFT, design::DANGER),
            (false, false, _) if hovered => (design::ACCENT_SOFT, design::ACCENT),
            (false, false, true) => (design::SHELL_LOW, design::DANGER),
            (false, false, false) => (design::SHELL_LOW, design::TEXT),
        };
        ui.painter
            .fill_rounded_rect(rect, ui.px(design::CHIP_RADIUS), fill);
        let label = if armed {
            "Confirm"
        } else {
            declaration.control_label()
        };
        ui.label_styled(rect, label, design::state_style(ui), ink, Align::Centre);
        if refused {
            continue;
        }
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
    if inner.h <= 0 {
        return;
    }
    let headline_height = ui.px(22);
    let line_height = ui.px(16);
    let Some(headline_band) = top_band(inner, headline_height) else {
        return;
    };
    match snapshot {
        None => ui.label(headline_band, "…", design::TEXT_FAINT, Align::Left),
        Some(snapshot) => {
            let colour = if snapshot.available {
                design::TEXT
            } else {
                design::TEXT_DIM
            };
            ui.label_styled(
                headline_band,
                &snapshot.headline,
                design::measurement_style(ui),
                colour,
                Align::Left,
            );
            if rung >= Representation::Wide {
                for (band, line) in snapshot_detail_bands(
                    inner,
                    headline_band,
                    line_height,
                    ui.px(6),
                    snapshot.detail.len(),
                )
                .into_iter()
                .zip(&snapshot.detail)
                {
                    ui.label_styled(
                        band,
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

/// The band one line of content occupies at the top of a tile's body.
///
/// `Ui::label_styled` centres its text vertically in whatever rect it is given, so handing it the
/// whole body leaves the one line worth reading floating in the middle of a tall tile — with any
/// following lines, which are anchored from the top, drawn *above* it. A tile reads top to bottom
/// like everything else, so content gets a band and not the body.
///
/// An empty-state message is the exception and is deliberately still centred: "empty — drop tiles
/// here" is about the space, not a reading taken in it.
///
/// `None` when the body is too short to hold the line at all, which is a tile that should draw
/// nothing rather than a clipped half-line.
pub(crate) fn top_band(inner: Rect, headline_height: i32) -> Option<Rect> {
    (inner.h >= headline_height)
        .then(|| Rect::new(inner.x, inner.y, inner.w, headline_height.min(inner.h)))
}

/// One band per detail line that fits under the headline, in order. A line that would be clipped
/// gets no band rather than half of one.
fn snapshot_detail_bands(
    inner: Rect,
    headline: Rect,
    line_height: i32,
    gap: i32,
    count: usize,
) -> Vec<Rect> {
    let mut bands = Vec::with_capacity(count);
    let mut y = headline.bottom() + gap;
    for _ in 0..count {
        if y + line_height > inner.bottom() {
            break;
        }
        bands.push(Rect::new(inner.x, y, inner.w, line_height));
        y += line_height;
    }
    bands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headline_sits_at_the_top_of_the_body_not_in_the_middle_of_it() {
        // A tall tile: the failing case was a headline centred in 180 pixels of body with the
        // detail lines drawn above it.
        let inner = Rect::new(10, 20, 280, 180);
        let headline = top_band(inner, 22).expect("a headline fits");
        assert_eq!(headline.y, inner.y);
        assert_eq!(headline.h, 22);
    }

    #[test]
    fn every_detail_line_falls_under_the_headline_in_order() {
        let inner = Rect::new(10, 20, 280, 180);
        let headline = top_band(inner, 22).unwrap();
        let bands = snapshot_detail_bands(inner, headline, 16, 6, 3);
        assert_eq!(bands.len(), 3);
        assert!(bands[0].y >= headline.bottom());
        assert!(bands[1].y >= bands[0].bottom());
        assert!(bands[2].y >= bands[1].bottom());
    }

    #[test]
    fn a_detail_line_that_would_be_clipped_is_left_out_rather_than_half_drawn() {
        // Room for the headline and one line, asked for four.
        let inner = Rect::new(10, 20, 280, 46);
        let headline = top_band(inner, 22).unwrap();
        let bands = snapshot_detail_bands(inner, headline, 16, 6, 4);
        assert_eq!(bands.len(), 1);
        assert!(bands[0].bottom() <= inner.bottom());
    }

    #[test]
    fn a_body_too_short_for_a_headline_draws_none() {
        assert!(top_band(Rect::new(0, 0, 100, 8), 22).is_none());
    }

    #[test]
    fn every_declared_scroll_route_names_a_real_action_and_only_levels_have_one() {
        for widget in WIDGETS {
            let Some((up, down)) = widget.scroll else {
                continue;
            };
            for id in [up, down] {
                assert!(
                    action::action(id).is_some(),
                    "{} scrolls to {id}, which is in no action manifest",
                    widget.id
                );
            }
            assert_ne!(
                up, down,
                "{}: a wheel must not do the same thing both ways",
                widget.id
            );
            // A wheel is a level's gesture. Anything else declaring one would be silently eating
            // the wheel over a tile that has nothing to step.
            assert_eq!(
                widget.id, "settings",
                "{} declares a scroll route but reports no level",
                widget.id
            );
        }
        // The one tile that reports a level does declare one: a person who has used any other
        // desktop will try it, and at the floor rung it is the only way to step without opening
        // Settings.
        assert!(
            descriptor("settings").unwrap().scroll.is_some(),
            "the settings widget has no wheel"
        );
    }

    /// The three separate readouts are gone, and nothing may quietly bring them back: a tile per
    /// hardware figure is the tray this desktop does not have.
    #[test]
    fn the_hardware_readings_share_one_widget_rather_than_one_each() {
        for gone in ["battery", "volume", "brightness"] {
            assert!(
                descriptor(gone).is_none(),
                "{gone} is a reading on the Settings widget, not a widget of its own"
            );
        }
        let settings = descriptor("settings").expect("the settings widget is declared");
        let ids: Vec<&str> = settings
            .regions
            .iter()
            .map(|region| region.action)
            .collect();
        assert!(ids.contains(&"volume.increase"));
        assert!(ids.contains(&"brightness.increase"));
    }

    #[test]
    fn a_readout_arrives_as_a_strip_not_a_list() {
        // Three readings and a control row need no more than a 3x2 strip along the bottom
        // rather than a second desktop.
        assert_eq!(descriptor("settings").unwrap().preferred_span, (3, 2));
    }

    #[test]
    fn a_widget_region_uses_the_short_control_when_the_heading_already_names_the_noun() {
        let settings = descriptor("settings").expect("declared");
        let open = settings.regions[0].declaration().expect("settings.open");
        assert_eq!(open.control_label(), "Open");
        let session = descriptor("session").expect("declared");
        let lock = session.regions[0].declaration().expect("session.lock");
        assert_eq!(lock.control_label(), "Lock");
    }

    /// The way in is the one control that survives every shrink: a tile too small for the steppers
    /// must still be able to reach the window that has them.
    #[test]
    fn the_way_into_settings_survives_to_the_floor() {
        let settings = descriptor("settings").expect("the settings widget is declared");
        let floor: Vec<&str> = regions_at(settings, Representation::Small)
            .iter()
            .map(|region| region.action)
            .collect();
        assert_eq!(floor, vec!["settings.open"]);
        let wide = regions_at(settings, Representation::Wide).len();
        let large = regions_at(settings, Representation::Large).len();
        assert!(floor.len() < wide && wide < large, "1 {wide} {large}");
    }

    /// A wheel over the body, put through the real `draw` the field screen calls.
    fn wheel_over(
        widget: &'static WidgetDescriptor,
        dy: f32,
        refused: &[String],
    ) -> Option<&'static str> {
        let mut buffer = pixelkit_raster::WindowBuffer::new(240, 160);
        let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
        let mut kernel = pixelkit_raster::RasterKernel::new();
        let mut input = pixelkit_shell::Input::new();
        let content = Rect::new(0, 0, 240, 160);
        input.cursor_moved(120.0, 40.0);
        input.scrolled(0.0, dy);
        let snapshot = WidgetSnapshot {
            headline: "45%".to_owned(),
            detail: Vec::new(),
            available: true,
            refused: refused.to_vec(),
        };
        let mut ui = Ui::new(
            pixelkit_raster::Painter::new(&mut buffer),
            &mut text,
            &mut input,
            crate::surface::design::theme(),
            pixelkit_shell::Scale::ONE,
            &mut kernel,
        );
        draw(
            &mut ui,
            content,
            widget,
            Some(&snapshot),
            Representation::Large,
            None,
        )
        .map(|declaration| declaration.id)
    }

    #[test]
    fn a_wheel_over_a_level_steps_it_the_way_round_a_person_expects() {
        let settings = descriptor("settings").unwrap();
        // Wheel up is a negative delta, and wheel up means more.
        assert_eq!(wheel_over(settings, -48.0, &[]), Some("volume.increase"));
        assert_eq!(wheel_over(settings, 48.0, &[]), Some("volume.decrease"));
        assert_eq!(wheel_over(settings, 0.0, &[]), None);
    }

    #[test]
    fn a_refused_level_does_not_step_on_a_wheel_either() {
        // A machine with no audio service refuses both step controls. The region strip already
        // draws them inert; the wheel has to honour the same answer, or the one route with no
        // visible control would be the one that still fired.
        let settings = descriptor("settings").unwrap();
        let refused = vec!["volume.increase".to_owned(), "volume.decrease".to_owned()];
        assert_eq!(wheel_over(settings, -48.0, &refused), None);
        assert_eq!(wheel_over(settings, 48.0, &refused), None);
        // With the hardware present the same wheel does step it.
        assert_eq!(wheel_over(settings, -48.0, &[]), Some("volume.increase"));
    }

    #[test]
    fn a_widget_with_no_wheel_route_leaves_the_wheel_alone() {
        for id in ["session", "notifications"] {
            assert_eq!(
                wheel_over(descriptor(id).unwrap(), -48.0, &[]),
                None,
                "{id}"
            );
        }
    }

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
        // Whatever else is dropped, the two a person reaches for must survive to the floor —
        // and they are the safe two.
        let names: Vec<&str> = regions_at(session, Representation::Small)
            .iter()
            .map(|region| region.action)
            .collect();
        assert!(names.contains(&"session.lock"));
        assert!(names.contains(&"session.logout"));
    }

    /// The floor is what every session tile shows until someone resizes it, so nothing that ends
    /// the machine may appear there. Getting this backwards is what put a working Shut down button
    /// on every desktop with no Log out beside it.
    #[test]
    fn nothing_that_ends_the_machine_is_reachable_at_the_floor() {
        let session = descriptor("session").expect("the session widget exists");
        for region in regions_at(session, Representation::Small) {
            assert!(
                !matches!(region.action, "session.poweroff" | "session.reboot"),
                "{} must not be offered at the floor rung",
                region.action
            );
        }
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

    /// logind saying "no" reaches the button, not only the paragraph beside it: a region the
    /// service has refused is drawn inert and a click on it is not taken. Drawing it dimmed while
    /// still firing would be the worst of both — it would look broken *and* act.
    #[test]
    fn a_refused_region_does_not_take_a_click() {
        let session = descriptor("session").expect("the session widget exists");
        let content = Rect::new(0, 0, 400, 300);
        let refused = vec!["session.poweroff".to_owned()];

        // At `Scale::ONE` a logical pixel is a device pixel, so the strip's geometry is what
        // `split_regions`/`region_rect` compute from `content` alone — no Ui needed to find where
        // to click, only to draw.
        let offered = regions_at(session, Representation::Large);
        let region_centre = |target: &str| {
            let index = offered
                .iter()
                .position(|region| region.action == target)
                .expect("the target region is offered at this rung");
            // `split_regions`' own arithmetic at `Scale::ONE`, where a logical pixel is a
            // device pixel and the strip is not clamped by this content's height.
            let strip = Rect::new(
                content.x,
                content.bottom() - REGION_STRIP_HEIGHT,
                content.w,
                REGION_STRIP_HEIGHT,
            );
            let rect = region_rect(strip, index, offered.len());
            (rect.x + rect.w / 2, rect.y + rect.h / 2)
        };

        let chosen = |refused: &[String], target: &str| {
            let (x, y) = region_centre(target);
            let mut buffer = pixelkit_raster::WindowBuffer::new(400, 300);
            let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
            let mut kernel = pixelkit_raster::RasterKernel::new();
            let mut input = pixelkit_shell::Input::new();
            input.cursor_moved(x as f32, y as f32);
            input.mouse(pixelkit_shell::MouseButton::Left, true);
            let mut ui = Ui::new(
                pixelkit_raster::Painter::new(&mut buffer),
                &mut text,
                &mut input,
                design::theme(),
                pixelkit_shell::Scale::ONE,
                &mut kernel,
            );
            draw_region_strip(
                &mut ui,
                content,
                session,
                Representation::Large,
                None,
                refused,
            )
            .1
            .map(|declaration| declaration.id)
        };

        assert_eq!(
            chosen(&[], "session.poweroff"),
            Some("session.poweroff"),
            "an unrefused region still fires"
        );
        assert_eq!(
            chosen(&refused, "session.poweroff"),
            None,
            "a refused region must not fire"
        );
        assert_eq!(
            chosen(&refused, "session.logout"),
            Some("session.logout"),
            "refusing one region leaves its neighbours working"
        );
    }
}
