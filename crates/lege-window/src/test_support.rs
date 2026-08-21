//! Test-only helpers shared across this crate's unit and property tests: loading topology
//! fixtures from `tests/fixtures/topologies/` and generating realistic multi-output topologies
//! (mixed scale, negative origins, hotplug sequences, overlapping and disjoint arrangements) for
//! proptest.

use crate::geometry::{Output, Rect};
use proptest::prelude::*;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct RectFixture {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Deserialize)]
struct OutputFixture {
    name: String,
    logical: RectFixture,
    scale_milli: u32,
}

#[derive(Deserialize)]
struct SequenceFixture {
    steps: Vec<Vec<OutputFixture>>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/topologies")
}

fn into_output(fixture: OutputFixture) -> Output {
    Output {
        name: fixture.name,
        logical: Rect::new(
            fixture.logical.x,
            fixture.logical.y,
            fixture.logical.width,
            fixture.logical.height,
        ),
        scale_milli: fixture.scale_milli,
    }
}

/// Load a single-topology fixture (a flat JSON array of outputs), e.g. `laptop-plus-hdmi`.
pub fn load_topology(name: &str) -> Vec<Output> {
    let path = fixtures_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading fixture {}: {error}", path.display()));
    let fixtures: Vec<OutputFixture> = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parsing fixture {}: {error}", path.display()));
    fixtures.into_iter().map(into_output).collect()
}

/// Load a hotplug-sequence fixture (`{"steps": [[...], [...], ...]}`), each step a full
/// topology snapshot in the order the outputs changed.
pub fn load_topology_sequence(name: &str) -> Vec<Vec<Output>> {
    let path = fixtures_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading fixture {}: {error}", path.display()));
    let sequence: SequenceFixture = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parsing fixture {}: {error}", path.display()));
    sequence
        .steps
        .into_iter()
        .map(|step| step.into_iter().map(into_output).collect())
        .collect()
}

/// One output with realistic bounds: mixed scale, coordinates that may be negative (an output
/// placed left of or above the primary).
pub fn arb_output() -> impl Strategy<Value = Output> {
    (
        -8000i32..8000,
        -8000i32..8000,
        200i32..7680,
        200i32..4320,
        prop_oneof![
            Just(1000u32),
            Just(1250),
            Just(1500),
            Just(1750),
            Just(2000)
        ],
    )
        .prop_map(|(x, y, width, height, scale)| Output {
            name: "out".into(),
            logical: Rect::new(x, y, width, height),
            scale_milli: scale,
        })
}

/// A window-sized rectangle, including ones already far off any plausible output.
pub fn arb_window() -> impl Strategy<Value = Rect> {
    (-20000i32..20000, -20000i32..20000, 1i32..6000, 1i32..4000)
        .prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
}

/// A non-empty topology of 1-4 outputs, covering hotplug arrangements from a single laptop
/// panel to a mixed-scale multi-monitor desk — overlapping and disjoint placements included,
/// since `arb_output` draws origins independently of one another.
pub fn arb_topology() -> impl Strategy<Value = Vec<Output>> {
    prop::collection::vec(arb_output(), 1..4)
}

/// A hotplug sequence: a starting topology followed by 1-4 further steps, each one output
/// added or removed relative to the previous step — connect, disconnect, connect a different
/// output, etc. Every step keeps at least one output, since a headless compositor is out of
/// scope here.
pub fn arb_hotplug_sequence() -> impl Strategy<Value = Vec<Vec<Output>>> {
    arb_topology().prop_flat_map(|initial| {
        prop::collection::vec(any::<bool>(), 1..4).prop_map(move |steps| {
            let mut topology = initial.clone();
            let mut sequence = vec![topology.clone()];
            for (index, add) in steps.into_iter().enumerate() {
                if add || topology.len() <= 1 {
                    topology.push(Output {
                        name: format!("hotplug-{index}"),
                        logical: Rect::new(
                            2000 + i32::try_from(index).unwrap_or(0) * 2000,
                            0,
                            1920,
                            1080,
                        ),
                        scale_milli: 1000,
                    });
                } else {
                    topology.remove(0);
                }
                sequence.push(topology.clone());
            }
            sequence
        })
    })
}
