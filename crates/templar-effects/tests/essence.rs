//! The essence planner's promises, checked on whole scenes.

use templar_effects::essence::{MAX_FIELD_CELLS, MOTE_BYTES, PATH_POINTS, Plan};
use templar_effects::{Planner, Rect, Request, Run, Sink, Style, Timing};

const SCREEN: Rect = Rect::new(0, 0, 1920, 1080);

/// A maximized browser over three terminals, leaving into the garden.
fn desktop(sink: Sink) -> Plan {
    let terminals = [
        Rect::new(80, 90, 700, 420),
        Rect::new(980, 140, 760, 460),
        Rect::new(420, 600, 820, 380),
    ];
    Planner::new().plan(&Request {
        screen: SCREEN,
        window: SCREEN,
        obstacles: &terminals,
        sink,
        timing: Timing::DEFAULT,
    })
}

fn texel(plan: &Plan, x: f32, y: f32) -> [u8; 4] {
    let col = ((x - plan.screen.x as f32) / plan.cell_px as f32)
        .floor()
        .clamp(0.0, (plan.cols - 1) as f32) as usize;
    let row = ((y - plan.screen.y as f32) / plan.cell_px as f32)
        .floor()
        .clamp(0.0, (plan.rows - 1) as f32) as usize;
    let at = (row * plan.cols as usize + col) * 4;
    plan.field[at..at + 4].try_into().unwrap()
}

fn unorm(v: u16) -> f32 {
    f32::from(v) / 65535.0
}

/// Where a mote starts, as the planner decides it: the centre of its piece (clipped to the
/// window), leaning half a pixel toward the piece's origin so a corner-centred piece has one
/// cell.
fn start(plan: &Plan, mote: &templar_effects::Mote) -> (f32, f32) {
    let (x0, y0) = point(plan, mote.source);
    let (x0, y0) = (x0.round(), y0.round());
    let piece = plan.piece_px as f32;
    let x1 = (x0 + piece).min(plan.window.right() as f32);
    let y1 = (y0 + piece).min(plan.window.bottom() as f32);
    ((x0 + x1) * 0.5 - 0.5, (y0 + y1) * 0.5 - 0.5)
}

fn point(plan: &Plan, p: [u16; 2]) -> (f32, f32) {
    (
        plan.screen.x as f32 + unorm(p[0]) * plan.screen.w as f32,
        plan.screen.y as f32 + unorm(p[1]) * plan.screen.h as f32,
    )
}

#[test]
fn covered_pieces_go_fast_and_garden_pieces_hold() {
    let plan = desktop(Sink::Garden);
    let fast = Timing::DEFAULT.covered_fraction();
    let mut covered = 0;
    let mut garden = 0;
    for (index, chunk) in plan.field.chunks_exact(4).enumerate() {
        let (r, g, a) = (
            f32::from(chunk[0]) / 255.0,
            f32::from(chunk[1]) / 255.0,
            chunk[3],
        );
        if a == 255 {
            covered += 1;
            assert!(
                r <= fast + 1.0 / 255.0,
                "covered cell {index} vanishes at {r}, after {fast}"
            );
        } else {
            garden += 1;
            assert!(
                g >= fast + 0.04,
                "garden cell {index} vanishes at {g}, too soon"
            );
        }
    }
    assert!(
        covered > 100 && garden > 100,
        "scene has both kinds: {covered} covered, {garden} garden"
    );
}

#[test]
fn every_mote_is_released_before_it_arrives_and_within_the_run() {
    for sink in [Sink::Garden, Sink::Card(Rect::new(900, 1010, 120, 60))] {
        let plan = desktop(sink);
        assert!(!plan.motes.is_empty());
        for mote in &plan.motes {
            assert!(mote.release < mote.arrive, "{sink:?}: {mote:?}");
        }
        assert_eq!(plan.mote_bytes().len(), plan.motes.len() * MOTE_BYTES);
    }
}

#[test]
fn covered_motes_leave_within_the_covered_time() {
    let plan = desktop(Sink::Garden);
    let fast = Timing::DEFAULT.covered_fraction();
    for mote in &plan.motes {
        let start = start(&plan, mote);
        if texel(&plan, start.0, start.1)[3] == 255 {
            assert!(
                unorm(mote.release) <= fast + 1e-3,
                "covered mote released at {}",
                unorm(mote.release)
            );
        }
    }
}

#[test]
fn garden_essence_soaks_in_a_gap_that_waits_for_it() {
    let plan = desktop(Sink::Garden);
    for mote in &plan.motes {
        let start = start(&plan, mote);
        if texel(&plan, start.0, start.1)[3] != 255 {
            continue;
        }
        let end = point(&plan, mote.path[PATH_POINTS - 1]);
        let end_texel = texel(&plan, end.0, end.1);
        assert_eq!(
            end_texel[3], 0,
            "covered essence must end in the garden, ended at {end:?}"
        );
        let held = f32::from(end_texel[1]) / 255.0;
        let arrive = unorm(mote.arrive).min(0.88);
        assert!(
            held >= arrive - 0.12,
            "gap at {end:?} lets go at {held}, essence arrives at {arrive}"
        );
    }
}

#[test]
fn paths_take_the_gap_in_a_wall() {
    // A wall across the screen with one gap; the card is below it.
    let gap = (1200, 1360);
    let wall = [
        Rect::new(0, 500, gap.0, 120),
        Rect::new(gap.1, 500, 1920 - gap.1, 120),
    ];
    let window = Rect::new(200, 100, 700, 300);
    let plan = Planner::new().plan(&Request {
        screen: SCREEN,
        window,
        obstacles: &wall,
        sink: Sink::Card(Rect::new(300, 960, 120, 80)),
        timing: Timing::DEFAULT,
    });
    let cell = plan.cell_px as f32;
    for mote in &plan.motes {
        let points: Vec<(f32, f32)> = mote.path.iter().map(|&p| point(&plan, p)).collect();
        // Where the path crosses the wall's middle line it is inside the gap.
        for pair in points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let y = 560.0;
            if (a.1 - y) * (b.1 - y) <= 0.0 && (a.1 - b.1).abs() > f32::EPSILON {
                let x = a.0 + (b.0 - a.0) * (y - a.1) / (b.1 - a.1);
                assert!(
                    x >= gap.0 as f32 - 2.0 * cell && x <= gap.1 as f32 + 2.0 * cell,
                    "a path crossed the wall at x = {x}, outside the gap"
                );
            }
        }
    }
    assert!(
        plan.bounds.right() >= gap.0,
        "the run's bounds include the detour through the gap"
    );
}

#[test]
fn card_paths_end_where_the_piece_sits_in_the_card() {
    let card = Rect::new(900, 1010, 120, 60);
    let plan = desktop(Sink::Card(card));
    for mote in &plan.motes {
        let end = point(&plan, mote.path[PATH_POINTS - 1]);
        let (sx, sy) = point(&plan, mote.path[0]);
        let expect = (
            card.x as f32 + sx / SCREEN.w as f32 * card.w as f32,
            card.y as f32 + sy / SCREEN.h as f32 * card.h as f32,
        );
        assert!(
            card.contains(end.0, end.1),
            "ended outside the card at {end:?}"
        );
        assert!(
            (end.0 - expect.0).abs() < 1.0 && (end.1 - expect.1).abs() < 1.0,
            "ended at {end:?}, expected {expect:?}"
        );
    }
}

#[test]
fn planning_is_deterministic_and_reuse_changes_nothing() {
    let terminals = [Rect::new(80, 90, 700, 420)];
    let request = Request {
        screen: SCREEN,
        window: Rect::new(300, 200, 900, 600),
        obstacles: &terminals,
        sink: Sink::Garden,
        timing: Timing::DEFAULT,
    };
    let mut planner = Planner::new();
    let first = planner.plan(&request);
    let mut reused = desktop(Sink::Garden);
    planner.plan_into(&request, &mut reused);
    assert_eq!(first.field, reused.field);
    assert_eq!(first.motes, reused.motes);
    assert_eq!(first.bounds, reused.bounds);
}

#[test]
fn the_field_is_bounded_at_any_resolution() {
    for screen in [
        Rect::new(0, 0, 3840, 2160),
        Rect::new(0, 0, 7680, 4320),
        Rect::new(0, 0, 640, 480),
    ] {
        let plan = Planner::new().plan(&Request {
            screen,
            window: screen,
            obstacles: &[],
            sink: Sink::Garden,
            timing: Timing::DEFAULT,
        });
        assert!(
            plan.cols.max(plan.rows) <= MAX_FIELD_CELLS,
            "{screen:?}: {}×{}",
            plan.cols,
            plan.rows
        );
        assert!(plan.motes.len() <= (MAX_FIELD_CELLS * MAX_FIELD_CELLS) as usize);
    }
}

#[test]
fn a_screen_with_no_garden_dissolves_in_place() {
    let plan = Planner::new().plan(&Request {
        screen: SCREEN,
        window: SCREEN,
        obstacles: &[SCREEN],
        sink: Sink::Garden,
        timing: Timing::DEFAULT,
    });
    assert!(
        plan.field.chunks_exact(4).all(|t| t[3] == 0),
        "nothing counts as covered"
    );
    assert!(!plan.motes.is_empty());
}

#[test]
fn degenerate_requests_draw_nothing() {
    let off = Planner::new().plan(&Request {
        screen: SCREEN,
        window: Rect::new(5000, 5000, 100, 100),
        obstacles: &[],
        sink: Sink::Garden,
        timing: Timing::DEFAULT,
    });
    assert!(off.motes.is_empty());
    let empty = Planner::new().plan(&Request {
        screen: SCREEN,
        window: Rect::new(10, 10, 0, 50),
        obstacles: &[],
        sink: Sink::Garden,
        timing: Timing::DEFAULT,
    });
    assert!(empty.motes.is_empty());
}

#[test]
fn timing_is_clamped_to_the_usable_range() {
    let t = Timing {
        duration_ms: 2000.0,
        covered_ms: 900.0,
    }
    .clamped();
    assert!((t.duration_ms - Timing::MAX_MS).abs() < f32::EPSILON);
    assert!(t.covered_ms <= t.duration_ms * 0.5);
    let t = Timing {
        duration_ms: f32::NAN,
        covered_ms: 1.0,
    }
    .clamped();
    assert!(t.duration_ms.is_finite() && t.covered_ms >= 8.0);
}

#[test]
fn a_run_reverses_without_a_jump() {
    let mut run = Run::depart(Timing {
        duration_ms: 200.0,
        covered_ms: 50.0,
    });
    assert!(run.advance(80.0));
    let at = run.u;
    run.reverse();
    assert!(
        (run.u - at).abs() < f32::EPSILON,
        "reversing keeps the state reached"
    );
    assert!(run.advance(40.0));
    assert!(run.u < at);
    assert!(!run.advance(1000.0));
    assert!(run.finished() && run.u <= 0.0);
}

#[test]
fn grain_size_reaches_the_shader_within_its_range() {
    let plan = desktop(Sink::Garden);
    // The `dust` vec4 follows screen, window, field, timing, glow and jitter.
    let grain = |style: Style| {
        let bytes = plan.uniform(0.5, &style);
        f32::from_le_bytes(bytes[96..100].try_into().unwrap())
    };
    assert_eq!(grain(Style::DEFAULT), Style::DEFAULT.grain_px);
    assert!(Style::DEFAULT.grain_px <= 2.0, "the default grain is dust");
    let with = |grain_px| Style {
        grain_px,
        ..Style::DEFAULT
    };
    assert_eq!(grain(with(4.0)), 4.0);
    assert_eq!(grain(with(0.0)), Style::MIN_GRAIN_PX);
    assert_eq!(grain(with(1000.0)), Style::MAX_GRAIN_PX);
    assert_eq!(grain(with(f32::NAN)), Style::DEFAULT.grain_px);
}
