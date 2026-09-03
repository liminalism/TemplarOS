//! Activation: making the installed build the running one.
//!
//! Installing is the easy half. The hard half is that "restart it" is a different instruction for
//! every component, and getting it wrong on the wrong one takes the desktop down. So the policy is
//! a pure function — [`plan`] turns a convergence into an ordered list of steps and nothing else —
//! and every effect goes through a trait ([`Systemctl`], [`BusProbe`], [`Clock`], [`Stamps`]) so
//! the whole engine is exercised against fakes rather than against a live session.
//!
//! Three rules carry most of the weight:
//!
//! * **Providers before dependents.** Restarting the shell into a sessiond that has not come back
//!   yet produces a shell full of dead proxies, which looks exactly like a shell bug.
//! * **A restart is not done until the new build answers.** The unit being `active` proves only
//!   that systemd started something. The gate waits for the restart counter to hold still and for
//!   the component's own stamp to say the new build set — the same evidence `lege dev status`
//!   uses, so the report cannot disagree with the deployment that produced it.
//! * **One retry, then on to the next.** A component that fails its gate twice is not going to
//!   succeed on the third try. It is still only that component: the packages are already
//!   installed, so the rest of the session is running new code against an old unit whether or not
//!   this tool restarts it, and stopping the run there leaves the shell — the thing being tested —
//!   on the previous build for a reason that has nothing to do with it. So every component is
//!   attempted, every outcome is reported, and the run ends by naming everything that failed and
//!   how to roll the whole set back.

use super::converge::{Convergence, Verdict};
use lege_build::{
    RunningStamp,
    components::{Activation, Component, Matrix, Scope},
};
use std::collections::BTreeMap;

/// What has to happen to one component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Step {
    /// Already running the target build, or nothing needs to run.
    Nothing { component: String, reason: String },
    /// Restart its unit and wait for the new build to answer.
    Restart {
        component: String,
        unit: String,
        scope: Scope,
        gate: HealthGate,
    },
    /// Running instances keep the old build; the next launch gets the new one.
    Relaunch { component: String },
    /// Restartable in principle, but not by this tool. Says what to type.
    Manual { component: String, unit: String },
    /// Log out and back in.
    NeedsSession { component: String },
    /// Reboot.
    NeedsReboot { component: String },
    /// Cannot be touched right now, and why.
    Blocked { component: String, reason: String },
}

impl Step {
    #[must_use]
    pub fn component(&self) -> &str {
        match self {
            Self::Nothing { component, .. }
            | Self::Restart { component, .. }
            | Self::Relaunch { component }
            | Self::Manual { component, .. }
            | Self::NeedsSession { component }
            | Self::NeedsReboot { component }
            | Self::Blocked { component, .. } => component,
        }
    }

    /// The one line a person reads.
    #[must_use]
    pub fn explanation(&self) -> String {
        match self {
            Self::Nothing { reason, .. } | Self::Blocked { reason, .. } => reason.clone(),
            Self::Restart { unit, .. } => format!("restart {unit}"),
            Self::Relaunch { .. } => {
                "open windows keep the old build; the next launch gets the new one".to_owned()
            }
            Self::Manual { unit, .. } if unit.is_empty() => {
                "restart it yourself when convenient".to_owned()
            }
            Self::Manual { unit, .. } => {
                format!("restart it yourself when convenient: systemctl --user restart {unit}")
            }
            Self::NeedsSession { .. } => "log out and back in".to_owned(),
            Self::NeedsReboot { .. } => "takes effect at the next boot".to_owned(),
        }
    }
}

/// What counts as a successful restart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthGate {
    /// How long the restart counter must hold still, in milliseconds.
    pub settle_ms: u64,
    /// The component's stamp must report this build set.
    pub expect_build_set: String,
    /// A bus name that must answer before the component counts as up — for the daemons where
    /// "the process exists" and "the service works" are genuinely different claims.
    pub bus_name: Option<String>,
}

/// How long a restarted unit's restart counter must hold still.
///
/// Three seconds because that is comfortably longer than the half-second a healthy shell takes to
/// come back, and short enough that a crash loop (which restarts far faster than that) is caught
/// within one window rather than after a deployment has moved on.
pub const SETTLE_MS: u64 = 3_000;

/// Everything the deployment would do, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationPlan {
    pub build_set: String,
    pub steps: Vec<Step>,
}

impl ActivationPlan {
    /// The steps that actually do something.
    #[must_use]
    pub fn restarts(&self) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|step| matches!(step, Step::Restart { .. }))
            .collect()
    }

    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, Step::Blocked { .. }))
    }
}

/// Conditions outside the convergence that change what may be done.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Conditions {
    /// `lege-hostd` has a transaction that has not reached a terminal phase. Restarting it would
    /// interrupt a staged kernel or driver update mid-flight.
    pub hostd_busy: bool,
    /// Components a pre-activation gate has ruled out of restarting this run, with the reason —
    /// an interface signature change, a schema skew. Keyed by component name.
    pub blocked: BTreeMap<String, String>,
}

/// Turn a convergence into an ordered activation plan.
///
/// Pure. Every judgement about what a component needs is made here, from the matrix and the
/// verdicts, so it can be read in one place and tested without a session.
#[must_use]
pub fn plan(matrix: &Matrix, convergence: &Convergence, conditions: &Conditions) -> ActivationPlan {
    let build_set = convergence.target.clone().unwrap_or_default();
    let verdicts: BTreeMap<&str, Verdict> = convergence
        .rows
        .iter()
        .map(|row| (row.component.as_str(), row.verdict))
        .collect();

    let steps = matrix
        .activation_order()
        .into_iter()
        .filter_map(|name| matrix.get(name))
        .map(|component| {
            step_for(
                component,
                verdicts.get(component.name.as_str()).copied(),
                conditions,
                &build_set,
            )
        })
        .collect();
    ActivationPlan { build_set, steps }
}

fn step_for(
    component: &Component,
    verdict: Option<Verdict>,
    conditions: &Conditions,
    build_set: &str,
) -> Step {
    let name = component.name.clone();
    match verdict {
        Some(Verdict::Converged) => Step::Nothing {
            component: name,
            reason: converged_reason(component),
        },
        Some(Verdict::NotInstalled) => Step::Blocked {
            component: name,
            reason: "not installed; the set is incomplete".to_owned(),
        },
        Some(Verdict::OtherVersion) => Step::Blocked {
            component: name,
            reason: "installed at another version; install the set before activating".to_owned(),
        },
        // NotRunning, NeedsActivation, Unverifiable, or a component the convergence did not
        // mention: whether to act is the matrix's call, below.
        _ => activation_step(component, conditions, build_set),
    }
}

/// Why a converged component needs nothing — which is not the same sentence for all of them.
///
/// A stamping daemon at the target set really is running this build. The boot-health probe and
/// the session units are merely *installed*; what makes them current is a boot or a login that
/// has not happened yet and does not need to be waited for. Saying "already running this build"
/// of a component that is not running at all would be a small, repeated lie in a report whose
/// whole value is that it can be believed.
fn converged_reason(component: &Component) -> String {
    if component.stamp.is_some() {
        return "already running this build".to_owned();
    }
    match component.activation {
        Activation::NeedsReboot => "installed; the next boot runs it".to_owned(),
        Activation::NeedsSession => "installed; the next session runs it".to_owned(),
        Activation::Relaunch => "installed; the next launch runs it".to_owned(),
        Activation::Manual => "installed; it runs the new build when next started".to_owned(),
        Activation::Immediate | Activation::Restart => {
            "installed; the next command you run is the new one".to_owned()
        }
    }
}

fn activation_step(component: &Component, conditions: &Conditions, build_set: &str) -> Step {
    let name = component.name.clone();
    if let Some(reason) = conditions.blocked.get(&name) {
        return Step::Blocked {
            component: name,
            reason: reason.clone(),
        };
    }
    match component.activation {
        Activation::Immediate => Step::Nothing {
            component: name,
            reason: "the next command you run is the new one".to_owned(),
        },
        Activation::Relaunch => Step::Relaunch { component: name },
        Activation::NeedsSession => Step::NeedsSession { component: name },
        Activation::NeedsReboot => Step::NeedsReboot { component: name },
        Activation::Manual => Step::Manual {
            component: name,
            unit: component.unit.clone().unwrap_or_default(),
        },
        Activation::Restart => {
            // hostd is the one component whose work can outlive the command that started it: a
            // staged kernel or driver transaction survives across reboots, and restarting the
            // daemon in the middle of one interrupts something a user is waiting on.
            if name == "hostd" && conditions.hostd_busy {
                return Step::Blocked {
                    component: name,
                    reason: "a host transaction is in flight; it will pick up the new build when \
                             that finishes"
                        .to_owned(),
                };
            }
            let Some(unit) = component.unit.clone() else {
                return Step::Manual {
                    component: name,
                    unit: String::new(),
                };
            };
            Step::Restart {
                component: name,
                unit,
                scope: component.scope,
                gate: HealthGate {
                    settle_ms: SETTLE_MS,
                    expect_build_set: build_set.to_owned(),
                    bus_name: bus_name_for(component),
                },
            }
        }
    }
}

/// The bus name a restarted component must answer on, where it has one.
///
/// Only for the components where the process existing and the service working are genuinely
/// different claims. `lege-resourced` is the example: it locks memory, opens its state directory
/// and starts its sampling thread before it claims `org.legeos.Resources`, so the bus name
/// answering is the evidence that it got all the way through startup and is ticking again.
fn bus_name_for(component: &Component) -> Option<String> {
    match component.name.as_str() {
        "resourced" => Some("org.legeos.Resources".to_owned()),
        "sessiond" => Some("org.legeos.Session".to_owned()),
        _ => None,
    }
}

/// Restarting units.
pub trait Systemctl: std::fmt::Debug {
    /// # Errors
    /// If systemd refuses the restart or cannot be reached.
    fn restart(&self, scope: Scope, unit: &str) -> anyhow::Result<()>;
    /// # Errors
    /// If the unit's state cannot be read.
    fn is_active(&self, scope: Scope, unit: &str) -> anyhow::Result<bool>;
    /// systemd's `NRestarts` — how many times it has restarted the unit itself. A count that
    /// keeps climbing after our restart is a crash loop.
    ///
    /// # Errors
    /// If the property cannot be read.
    fn restart_count(&self, scope: Scope, unit: &str) -> anyhow::Result<u32>;
}

/// Asking whether a service answers.
pub trait BusProbe: std::fmt::Debug {
    fn answers(&self, bus_name: &str) -> bool;
}

/// Time, so the settle window is not a real three seconds in a test.
pub trait Clock: std::fmt::Debug {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, millis: u64);
}

/// Reading the runtime stamps.
pub trait Stamps: std::fmt::Debug {
    fn read(&self) -> BTreeMap<String, RunningStamp>;
}

/// What actually happened to one step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// Nothing to do.
    Skipped,
    /// Restarted, and the new build answered.
    Activated,
    /// Restarted twice and never came back healthy.
    Failed(String),
    /// Left for the person: a logout, a reboot, a manual restart, a block.
    Deferred(String),
}

/// The result of running a plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationReport {
    pub build_set: String,
    pub outcomes: Vec<(String, StepOutcome)>,
}

impl ActivationReport {
    /// Every component that was restarted and did not come back, in the order they were tried.
    ///
    /// All of them, not the first: once the run stopped abandoning the list at the first failure,
    /// a deployment can end with more than one thing broken, and a final line that named only one
    /// of them would send someone to fix that and miss the rest.
    #[must_use]
    pub fn failures(&self) -> Vec<(&str, &str)> {
        self.outcomes
            .iter()
            .filter_map(|(component, outcome)| match outcome {
                StepOutcome::Failed(reason) => Some((component.as_str(), reason.as_str())),
                _ => None,
            })
            .collect()
    }
}

/// The effects an activation needs.
#[derive(Debug)]
pub struct Effects<'a> {
    pub systemctl: &'a dyn Systemctl,
    pub bus: &'a dyn BusProbe,
    pub clock: &'a dyn Clock,
    pub stamps: &'a dyn Stamps,
}

/// Runs a plan, all of it.
///
/// Every step is attempted even after one has failed. The install is already done by the time
/// activation starts, so a component that will not restart is stale either way; refusing to
/// restart the ones after it does not undo anything, it only adds more stale components — and the
/// shell, which is what a person deploying is almost always here to look at, sits at the end of
/// the order behind everything else. The report says what each one did, and the caller still
/// fails the deployment.
#[must_use]
pub fn execute(plan: &ActivationPlan, matrix: &Matrix, effects: &Effects<'_>) -> ActivationReport {
    let mut outcomes = Vec::new();
    for step in &plan.steps {
        let component = step.component().to_owned();
        let outcome = match step {
            Step::Nothing { .. } => StepOutcome::Skipped,
            Step::Manual { unit, .. } => manual_step_outcome(matrix, &component, unit, effects)
                .unwrap_or_else(|| StepOutcome::Deferred(step.explanation())),
            Step::Relaunch { .. }
            | Step::NeedsSession { .. }
            | Step::NeedsReboot { .. }
            | Step::Blocked { .. } => StepOutcome::Deferred(step.explanation()),
            Step::Restart {
                unit, scope, gate, ..
            } => restart_with_gate(matrix, &component, unit, *scope, gate, effects),
        };
        outcomes.push((component, outcome));
    }
    ActivationReport {
        build_set: plan.build_set.clone(),
        outcomes,
    }
}

/// Whether a component this deployment deliberately does not restart is nevertheless broken.
///
/// `lege-sessiond` is the case this exists for. It is `manual` because restarting it disconnects
/// the session, so a deployment installs its new build and leaves the running one alone — and a
/// deployment into a session whose policy daemon has already died would otherwise print a page of
/// ticks and the word "succeeded". `Some(Failed)` only when the unit systemd knows about is
/// *active* and its bus name does not answer: exactly the "running but not working" claim the
/// restart gate makes, and never a complaint about a daemon that is simply not running, which for
/// a manual component is ordinary (a root deployment with no session, a machine at a greeter).
fn manual_step_outcome(
    matrix: &Matrix,
    component: &str,
    unit: &str,
    effects: &Effects<'_>,
) -> Option<StepOutcome> {
    let declared = matrix.components.iter().find(|c| c.name == component)?;
    let bus_name = bus_name_for(declared)?;
    if unit.is_empty() || !effects.systemctl.is_active(declared.scope, unit).ok()? {
        return None;
    }
    (!effects.bus.answers(&bus_name)).then(|| {
        StepOutcome::Failed(format!(
            "{unit} is running but {bus_name} does not answer; this session is not healthy and \
             the new build was not activated into it"
        ))
    })
}

/// Restart, gate, and on failure restart once more and gate again.
fn restart_with_gate(
    matrix: &Matrix,
    component: &str,
    unit: &str,
    scope: Scope,
    gate: &HealthGate,
    effects: &Effects<'_>,
) -> StepOutcome {
    let mut last = String::new();
    for attempt in 1..=2 {
        if let Err(error) = effects.systemctl.restart(scope, unit) {
            last = format!("systemd refused to restart {unit}: {error}");
            continue;
        }
        match check_gate(matrix, component, unit, scope, gate, effects) {
            Ok(()) => return StepOutcome::Activated,
            Err(reason) => {
                last = if attempt == 1 {
                    format!("{reason}; retried once")
                } else {
                    reason
                };
            }
        }
    }
    StepOutcome::Failed(last)
}

/// The gate itself: the unit is up, its restart counter has stopped moving, its bus name answers
/// if it has one, and its own stamp reports the build set being deployed.
fn check_gate(
    matrix: &Matrix,
    component: &str,
    unit: &str,
    scope: Scope,
    gate: &HealthGate,
    effects: &Effects<'_>,
) -> Result<(), String> {
    let before = effects
        .systemctl
        .restart_count(scope, unit)
        .map_err(|error| format!("could not read {unit}'s restart count: {error}"))?;
    effects.clock.sleep_ms(gate.settle_ms);
    let after = effects
        .systemctl
        .restart_count(scope, unit)
        .map_err(|error| format!("could not read {unit}'s restart count: {error}"))?;
    if after != before {
        return Err(format!(
            "{unit} restarted {} more time(s) while settling — it is crash-looping",
            after.saturating_sub(before)
        ));
    }
    if !effects
        .systemctl
        .is_active(scope, unit)
        .map_err(|error| format!("could not read {unit}'s state: {error}"))?
    {
        return Err(format!("{unit} is not active"));
    }
    if let Some(bus_name) = &gate.bus_name
        && !effects.bus.answers(bus_name)
    {
        return Err(format!("{unit} is running but {bus_name} does not answer"));
    }
    let Some(stamp_name) = matrix.get(component).and_then(|c| c.stamp.clone()) else {
        return Ok(());
    };
    match effects.stamps.read().get(&stamp_name) {
        Some(stamp) if stamp.build_set == gate.expect_build_set => Ok(()),
        Some(stamp) => Err(format!(
            "{unit} came back running {} instead of {}",
            stamp.build_set, gate.expect_build_set
        )),
        None => Err(format!("{unit} came back without writing a build stamp")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::converge::{Observation, converge};
    use super::*;
    use std::{
        cell::RefCell,
        collections::{BTreeSet, HashMap},
    };

    const SET: &str = "0.1.0~dev2000+gaaaaaaa";
    const OLDER: &str = "0.1.0~dev1000+gbbbbbbb";

    fn stamp(build_set: &str) -> RunningStamp {
        RunningStamp {
            component: String::new(),
            build_set: build_set.to_owned(),
            git_sha: "aaaaaaa".to_owned(),
            dirty: false,
            pid: 1,
            started_at_ms: 0,
            schema_versions: BTreeMap::new(),
        }
    }

    /// Everything installed at `SET`; the named components still running `OLDER`.
    fn convergence(stale: &[&str]) -> Convergence {
        let matrix = Matrix::embedded();
        let mut observation = Observation::default();
        for component in &matrix.components {
            observation
                .installed
                .insert(component.package.clone(), SET.to_owned());
            if let Some(name) = &component.stamp {
                let running = if stale.contains(&component.name.as_str()) {
                    OLDER
                } else {
                    SET
                };
                observation.running.insert(name.clone(), stamp(running));
            }
        }
        converge(&matrix, &observation, None)
    }

    fn step(plan: &ActivationPlan, component: &str) -> Step {
        plan.steps
            .iter()
            .find(|step| step.component() == component)
            .unwrap_or_else(|| panic!("no step for {component}"))
            .clone()
    }

    #[test]
    fn a_converged_machine_has_nothing_to_activate() {
        let plan = plan(
            &Matrix::embedded(),
            &convergence(&[]),
            &Conditions::default(),
        );
        assert!(plan.restarts().is_empty(), "{plan:#?}");
    }

    #[test]
    fn a_stale_shell_is_restarted_and_gated_on_its_own_stamp() {
        let plan = plan(
            &Matrix::embedded(),
            &convergence(&["shell"]),
            &Conditions::default(),
        );
        let Step::Restart { unit, gate, .. } = step(&plan, "shell") else {
            panic!("the shell must be restarted");
        };
        assert_eq!(unit, "lege-shell.service");
        assert_eq!(gate.expect_build_set, SET);
        assert_eq!(gate.settle_ms, SETTLE_MS);
    }

    #[test]
    fn providers_are_activated_before_their_dependents() {
        let plan = plan(
            &Matrix::embedded(),
            &convergence(&["shell", "sessiond", "filed"]),
            &Conditions::default(),
        );
        let order: Vec<&str> = plan.steps.iter().map(Step::component).collect();
        for provider in ["sessiond", "filed", "comp"] {
            let their = order.iter().position(|name| *name == provider).unwrap();
            let shell = order.iter().position(|name| *name == "shell").unwrap();
            assert!(their < shell, "{provider} must precede shell in {order:?}");
        }
    }

    #[test]
    fn the_daemons_that_would_disconnect_the_shell_are_left_to_the_person() {
        let plan = plan(
            &Matrix::embedded(),
            &convergence(&["sessiond", "filed", "portal"]),
            &Conditions::default(),
        );
        for component in ["sessiond", "filed", "portal"] {
            assert!(
                matches!(step(&plan, component), Step::Manual { .. }),
                "{component} must not be restarted by the tool yet"
            );
        }
    }

    #[test]
    fn the_compositor_is_never_restarted() {
        let plan = plan(
            &Matrix::embedded(),
            &convergence(&["comp"]),
            &Conditions::default(),
        );
        assert!(matches!(step(&plan, "comp"), Step::NeedsSession { .. }));
    }

    #[test]
    fn hostd_is_blocked_while_a_transaction_is_in_flight() {
        let conditions = Conditions {
            hostd_busy: true,
            ..Conditions::default()
        };
        let plan = plan(&Matrix::embedded(), &convergence(&["hostd"]), &conditions);
        let Step::Blocked { reason, .. } = step(&plan, "hostd") else {
            panic!("a busy hostd must not be restarted");
        };
        assert!(reason.contains("transaction"), "{reason}");
    }

    #[test]
    fn a_gate_can_rule_a_component_out_of_this_run() {
        let mut blocked = BTreeMap::new();
        blocked.insert(
            "shell".to_owned(),
            "an interface signature changed; this needs a new session".to_owned(),
        );
        let conditions = Conditions {
            blocked,
            ..Conditions::default()
        };
        let plan = plan(&Matrix::embedded(), &convergence(&["shell"]), &conditions);
        assert!(matches!(step(&plan, "shell"), Step::Blocked { .. }));
    }

    #[test]
    fn an_incomplete_install_blocks_rather_than_restarting_into_it() {
        let matrix = Matrix::embedded();
        let mut observation = Observation::default();
        for component in &matrix.components {
            if component.name == "filed" {
                continue;
            }
            observation
                .installed
                .insert(component.package.clone(), SET.to_owned());
            if let Some(name) = &component.stamp {
                observation.running.insert(name.clone(), stamp(OLDER));
            }
        }
        let convergence = converge(&matrix, &observation, None);
        let plan = plan(&matrix, &convergence, &Conditions::default());
        let Step::Blocked { reason, .. } = step(&plan, "filed") else {
            panic!("a missing package must block");
        };
        assert!(reason.contains("not installed"), "{reason}");
    }

    // ---- execute ----

    #[derive(Debug, Default)]
    struct FakeSystem {
        restarts: RefCell<Vec<String>>,
        /// Units whose restart counter keeps climbing while settling — a crash loop.
        looping: RefCell<BTreeSet<String>>,
        inactive: RefCell<BTreeSet<String>>,
        /// Units systemd will not restart at all — the hostd failure this tool met in practice.
        refused: RefCell<BTreeSet<String>>,
        counter: RefCell<HashMap<String, u32>>,
    }

    impl Systemctl for FakeSystem {
        fn restart(&self, _scope: Scope, unit: &str) -> anyhow::Result<()> {
            if self.refused.borrow().contains(unit) {
                anyhow::bail!("Unit {unit} failed to load");
            }
            self.restarts.borrow_mut().push(unit.to_owned());
            Ok(())
        }
        fn is_active(&self, _scope: Scope, unit: &str) -> anyhow::Result<bool> {
            Ok(!self.inactive.borrow().contains(unit))
        }
        fn restart_count(&self, _scope: Scope, unit: &str) -> anyhow::Result<u32> {
            let mut counter = self.counter.borrow_mut();
            let value = counter.entry(unit.to_owned()).or_insert(0);
            if self.looping.borrow().contains(unit) {
                *value += 1;
            }
            Ok(*value)
        }
    }

    #[derive(Debug, Default)]
    struct FakeBus {
        silent: BTreeSet<String>,
    }
    impl BusProbe for FakeBus {
        fn answers(&self, bus_name: &str) -> bool {
            !self.silent.contains(bus_name)
        }
    }

    #[derive(Debug, Default)]
    struct FakeClock;
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            0
        }
        fn sleep_ms(&self, _millis: u64) {}
    }

    #[derive(Debug)]
    struct FakeStamps(BTreeMap<String, RunningStamp>);
    impl Stamps for FakeStamps {
        fn read(&self) -> BTreeMap<String, RunningStamp> {
            self.0.clone()
        }
    }

    fn stamps_at(build_set: &str) -> FakeStamps {
        let mut map = BTreeMap::new();
        for component in &Matrix::embedded().components {
            if let Some(name) = &component.stamp {
                map.insert(name.clone(), stamp(build_set));
            }
        }
        FakeStamps(map)
    }

    fn outcome(report: &ActivationReport, component: &str) -> StepOutcome {
        report
            .outcomes
            .iter()
            .find(|(name, _)| name == component)
            .map_or_else(
                || panic!("no outcome for {component}"),
                |(_, outcome)| outcome.clone(),
            )
    }

    #[test]
    fn a_restart_whose_new_build_answers_is_an_activation() {
        let matrix = Matrix::embedded();
        let plan = plan(&matrix, &convergence(&["shell"]), &Conditions::default());
        let system = FakeSystem::default();
        let stamps = stamps_at(SET);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &FakeBus::default(),
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        assert_eq!(outcome(&report, "shell"), StepOutcome::Activated);
        assert_eq!(system.restarts.borrow().as_slice(), ["lege-shell.service"]);
        assert!(report.failures().is_empty());
    }

    #[test]
    fn a_unit_that_comes_back_running_the_old_build_is_not_activated() {
        // The failure this gate exists for: systemd reports the unit active, and it is — running
        // the binary that was already there.
        let matrix = Matrix::embedded();
        let plan = plan(&matrix, &convergence(&["shell"]), &Conditions::default());
        let system = FakeSystem::default();
        let stamps = stamps_at(OLDER);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &FakeBus::default(),
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        let StepOutcome::Failed(reason) = outcome(&report, "shell") else {
            panic!("a stale build must fail the gate");
        };
        assert!(reason.contains("instead of"), "{reason}");
        assert_eq!(system.restarts.borrow().len(), 2, "one retry, and only one");
    }

    #[test]
    fn a_crash_looping_unit_fails_its_gate() {
        let matrix = Matrix::embedded();
        let plan = plan(&matrix, &convergence(&["shell"]), &Conditions::default());
        let system = FakeSystem::default();
        system
            .looping
            .borrow_mut()
            .insert("lege-shell.service".to_owned());
        let stamps = stamps_at(SET);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &FakeBus::default(),
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        let StepOutcome::Failed(reason) = outcome(&report, "shell") else {
            panic!("a crash loop must fail the gate");
        };
        assert!(reason.contains("crash-looping"), "{reason}");
    }

    #[test]
    fn a_daemon_whose_bus_name_does_not_answer_fails_even_though_the_unit_is_active() {
        let matrix = Matrix::embedded();
        let plan = plan(
            &matrix,
            &convergence(&["resourced"]),
            &Conditions::default(),
        );
        let system = FakeSystem::default();
        let bus = FakeBus {
            silent: ["org.legeos.Resources".to_owned()].into_iter().collect(),
        };
        let stamps = stamps_at(SET);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &bus,
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        let StepOutcome::Failed(reason) = outcome(&report, "resourced") else {
            panic!("a silent governor must fail the gate");
        };
        assert!(reason.contains("does not answer"), "{reason}");
    }

    #[test]
    fn a_failure_does_not_keep_the_shell_from_being_restarted() {
        // resourced sorts before shell in the matrix. The shell is what a person deploying is
        // here to look at; a governor that will not come back is its own problem and must not
        // silently leave the shell on the previous build.
        let matrix = Matrix::embedded();
        let plan = plan(
            &matrix,
            &convergence(&["resourced", "shell"]),
            &Conditions::default(),
        );
        let system = FakeSystem::default();
        system
            .looping
            .borrow_mut()
            .insert("lege-resourced.service".to_owned());
        let stamps = stamps_at(SET);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &FakeBus::default(),
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        assert!(matches!(
            outcome(&report, "resourced"),
            StepOutcome::Failed(_)
        ));
        assert_eq!(outcome(&report, "shell"), StepOutcome::Activated);
        assert!(
            system
                .restarts
                .borrow()
                .contains(&"lege-shell.service".to_owned()),
            "the shell must still be restarted after an earlier failure"
        );
        assert_eq!(
            report
                .failures()
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            ["resourced"]
        );
    }

    #[test]
    fn every_failure_is_reported_not_just_the_first() {
        let matrix = Matrix::embedded();
        let plan = plan(
            &matrix,
            &convergence(&["resourced", "shell"]),
            &Conditions::default(),
        );
        let system = FakeSystem::default();
        for unit in ["lege-resourced.service", "lege-shell.service"] {
            system.looping.borrow_mut().insert(unit.to_owned());
        }
        let stamps = stamps_at(SET);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &FakeBus::default(),
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        assert_eq!(
            report
                .failures()
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            ["resourced", "shell"]
        );
    }

    #[test]
    fn a_restart_systemd_refuses_still_leaves_the_rest_attempted() {
        // The shape of the deployment that produced this change: hostd's unit could not be
        // restarted at all, and nine components after it were reported "not attempted" even
        // though every package was already installed.
        let matrix = Matrix::embedded();
        let plan = plan(
            &matrix,
            &convergence(&["hostd", "shell"]),
            &Conditions::default(),
        );
        let system = FakeSystem::default();
        system
            .refused
            .borrow_mut()
            .insert("lege-hostd.service".to_owned());
        let stamps = stamps_at(SET);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &FakeBus::default(),
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        let StepOutcome::Failed(reason) = outcome(&report, "hostd") else {
            panic!("a refused restart must fail");
        };
        assert!(reason.contains("refused"), "{reason}");
        assert_eq!(outcome(&report, "shell"), StepOutcome::Activated);
    }

    #[test]
    fn a_session_daemon_that_is_running_but_silent_fails_the_deployment_and_names_the_rollback() {
        // sessiond is `manual`: a deployment installs its new build and deliberately does not
        // restart it, because restarting it disconnects the session. That must not mean a
        // deployment into a session whose policy daemon has already died reports success.
        let matrix = Matrix::embedded();
        let plan = plan(&matrix, &convergence(&["sessiond"]), &Conditions::default());
        assert!(
            matches!(step(&plan, "sessiond"), Step::Manual { .. }),
            "sessiond is still not restarted by a deployment"
        );

        let system = FakeSystem::default();
        let bus = FakeBus {
            silent: ["org.legeos.Session".to_owned()].into_iter().collect(),
        };
        let stamps = stamps_at(SET);
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &bus,
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        let StepOutcome::Failed(reason) = outcome(&report, "sessiond") else {
            panic!("a running-but-silent sessiond must fail the deployment");
        };
        assert!(reason.contains("does not answer"), "{reason}");
        assert!(
            system.restarts.borrow().is_empty(),
            "and it must fail without anyone restarting it"
        );

        // The failure carries the sentence a person acts on: which set to go back to.
        let advice = crate::dev::report::render_rollback_advice(&report.failures(), Some(OLDER));
        assert!(advice.contains("sessiond did not come back"), "{advice}");
        assert!(advice.contains(OLDER), "{advice}");
        assert!(advice.contains("sudo lege dev rollback"), "{advice}");

        // A machine where sessiond is simply not running — a root deployment with no session, a
        // machine sitting at a greeter — is not a failure, and must not be reported as one.
        let system = FakeSystem::default();
        system
            .inactive
            .borrow_mut()
            .insert("lege-sessiond.service".to_owned());
        let report = execute(
            &plan,
            &matrix,
            &Effects {
                systemctl: &system,
                bus: &bus,
                clock: &FakeClock,
                stamps: &stamps,
            },
        );
        assert!(
            matches!(outcome(&report, "sessiond"), StepOutcome::Deferred(_)),
            "a daemon that is not running is not a broken one"
        );
    }
}
