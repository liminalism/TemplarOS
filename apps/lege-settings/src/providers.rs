//! Providers page (`org.legeos.Providers1`): which application fills each exclusive system
//! role, and why.
//!
//! `Report()` returns `(role, resolution_json)` pairs. [`Resolution`] mirrors the wire shape
//! that `crates/lege-session-core/src/providers.rs`'s `Resolution` enum serializes to. The JSON
//! is the contract between sessiond and every client — this crate keeps its own copy of the
//! shape rather than depending on `lege-session-core` (which also drags in `lege-state`/SQLite
//! for no reason a settings page needs).

use anyhow::{Context, Result};
use lege_types::{AppId, ProviderRole};
use serde::Deserialize;

/// Mirrors `lege_session_core::providers::Resolution`'s `#[serde(tag = "kind")]` wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub(crate) enum Resolution {
    /// Explicitly selected and installed.
    Selected { app_id: AppId },
    /// No explicit selection: highest-priority declaration wins.
    DefaultedByPriority {
        app_id: AppId,
        competitors: Vec<AppId>,
    },
    /// Selected app is not installed / no longer declares the role.
    SelectionStale { app_id: AppId },
    /// Nothing declares this role.
    NoProvider,
}

/// One full sentence explaining a role's resolution, for a person. Pure and unit-testable
/// against literal [`Resolution`] values — no D-Bus call needed.
///
/// `DefaultedByPriority::competitors` carries only the losing app IDs, not their priorities (see
/// `Resolution`'s doc comment in `lege-session-core`), so the sentence is phrased around what is
/// actually known rather than inventing a priority number.
pub(crate) fn explain(role: ProviderRole, resolution: &Resolution) -> String {
    match resolution {
        Resolution::Selected { app_id } => format!("{app_id} is selected for {role}."),
        Resolution::DefaultedByPriority {
            app_id,
            competitors,
        } if competitors.is_empty() => {
            format!(
                "{app_id} is the default for {role} by priority (no other application declares it)."
            )
        }
        Resolution::DefaultedByPriority {
            app_id,
            competitors,
        } => {
            format!(
                "{app_id} is the default for {role} by priority, ahead of {}.",
                join_app_ids(competitors)
            )
        }
        Resolution::SelectionStale { app_id } => format!(
            "{app_id} was selected for {role}, but no longer declares it (uninstalled, or its declaration was withdrawn)."
        ),
        Resolution::NoProvider => format!("No application declares {role}."),
    }
}

fn join_app_ids(ids: &[AppId]) -> String {
    ids.iter().map(AppId::as_str).collect::<Vec<_>>().join(", ")
}

/// The bus operations this page needs.
pub(crate) trait ProvidersSource {
    async fn report(&self) -> Result<Vec<(String, String)>>;
    async fn candidates(&self, role: &str) -> Result<Vec<(String, u32)>>;
    async fn select(&self, role: &str, app_id: &str) -> Result<()>;
    async fn deselect(&self, role: &str) -> Result<()>;
}

impl ProvidersSource for lege_client::proxies::ProvidersProxy<'_> {
    async fn report(&self) -> Result<Vec<(String, String)>> {
        self.report().await.context("Providers1.Report")
    }
    async fn candidates(&self, role: &str) -> Result<Vec<(String, u32)>> {
        self.candidates(role).await.context("Providers1.Candidates")
    }
    async fn select(&self, role: &str, app_id: &str) -> Result<()> {
        self.select(role, app_id).await.context("Providers1.Select")
    }
    async fn deselect(&self, role: &str) -> Result<()> {
        self.deselect(role).await.context("Providers1.Deselect")
    }
}

/// One role as the graphical page shows it: the sentence, the choice, and where the choice
/// currently sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoleChoice {
    pub(crate) role: ProviderRole,
    /// The sentence [`explain`] writes for the current resolution.
    pub(crate) explanation: String,
    /// What the chooser offers, in the order it offers them. `Automatic` is always first: it is
    /// what a role starts as, and it is the answer a person wants when they have made a mess.
    pub(crate) options: Vec<ChoiceOption>,
    /// Which option is in force. Always a valid index into `options`.
    pub(crate) selected: usize,
}

/// One entry in a role's chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChoiceOption {
    /// Let priority decide — `Providers1.Deselect`.
    Automatic,
    /// Pin this application — `Providers1.Select`.
    App { app_id: String, priority: u32 },
}

impl ChoiceOption {
    /// The text the chooser shows for every option in one role's list.
    ///
    /// Per list rather than per option, because whether the id belongs in a row is a property of
    /// the list: `AppNames::qualified` appends it so that two installed copies of Okular are
    /// distinguishable, and where there is only one Okular it is noise that costs the row its
    /// name. At the chooser's width "PCManFM-Qt File Manager
    /// (desktop-import.pcmanfm-qt.desktop) · priority 50" elided inside the parenthesis, so the
    /// row said neither which application it was nor what its priority was. So: the plain name
    /// unless another option in the same list carries it too.
    ///
    /// Priority is spelled out either way — it is the reason an application is offered where it
    /// is, and a bare list of identifiers explains nothing.
    pub(crate) fn labels(options: &[Self], names: &crate::names::AppNames) -> Vec<String> {
        let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for option in options {
            if let Self::App { app_id, .. } = option {
                *seen.entry(names.name(app_id)).or_default() += 1;
            }
        }
        options
            .iter()
            .map(|option| match option {
                Self::Automatic => "Choose automatically".to_owned(),
                Self::App { app_id, priority } => {
                    let name = names.name(app_id);
                    let shown = if seen.get(name).copied().unwrap_or(0) > 1 {
                        names.qualified(app_id)
                    } else {
                        name.to_owned()
                    };
                    format!("{shown} · priority {priority}")
                }
            })
            .collect()
    }
}

/// Build one role's chooser from its resolution and its candidate list.
///
/// The selected index is derived from the resolution rather than from the candidate list, so the
/// two disagreeing — a selection whose application has since stopped declaring the role, which
/// `Report` calls `SelectionStale` — shows as *Automatic* being not-yet-true rather than as a
/// silent pick of some other application. `explain` has already said what went wrong in words.
pub(crate) fn role_choice(
    role: ProviderRole,
    resolution: &Resolution,
    candidates: &[(String, u32)],
) -> RoleChoice {
    let mut options = vec![ChoiceOption::Automatic];
    options.extend(
        candidates
            .iter()
            .map(|(app_id, priority)| ChoiceOption::App {
                app_id: app_id.clone(),
                priority: *priority,
            }),
    );

    // Only an explicit selection moves the chooser off Automatic. A role resolved by priority is
    // *on* automatic, however emphatically one application happens to be winning.
    let pinned = match resolution {
        Resolution::Selected { app_id } | Resolution::SelectionStale { app_id } => {
            Some(app_id.as_str())
        }
        Resolution::DefaultedByPriority { .. } | Resolution::NoProvider => None,
    };
    let selected = pinned
        .and_then(|pinned| {
            options.iter().position(
                |option| matches!(option, ChoiceOption::App { app_id, .. } if app_id == pinned),
            )
        })
        .unwrap_or(0);

    RoleChoice {
        role,
        explanation: explain(role, resolution),
        options,
        selected,
    }
}

/// Every role's chooser, in `ProviderRole::ALL` order.
///
/// A role whose candidates cannot be read still gets a row — with the sentence, and a chooser
/// holding only Automatic — because the sentence is the part that explains the machine, and
/// dropping the row would make a failing service look like a role that does not exist.
pub(crate) async fn role_choices_with<S: ProvidersSource>(
    source: &S,
) -> Result<Vec<RoleChoice>, String> {
    let pairs = match source.report().await {
        Ok(pairs) => pairs,
        Err(error) => return Err(format!("service unavailable: {error:#}")),
    };
    let report = parse_report(&pairs);
    let mut choices = Vec::with_capacity(ProviderRole::ALL.len());
    for role in ProviderRole::ALL {
        let resolution = report
            .iter()
            .find(|(reported, _)| *reported == role)
            .map_or(Resolution::NoProvider, |(_, resolution)| resolution.clone());
        let candidates = source.candidates(role.as_str()).await.unwrap_or_default();
        choices.push(role_choice(role, &resolution, &candidates));
    }
    Ok(choices)
}

/// Render every role in [`ProviderRole::ALL`] order (or just `role`, if given) against a live or
/// fake [`ProvidersSource`]. Never panics: a `Report()` failure or an unparseable resolution
/// becomes a line in the output, not an aborted page.
pub(crate) async fn render_with<S: ProvidersSource>(
    source: &S,
    role: Option<ProviderRole>,
) -> String {
    match source.report().await {
        Ok(pairs) => render_report(&pairs, role),
        Err(error) => format!("service unavailable: {error:#}"),
    }
}

fn render_report(pairs: &[(String, String)], filter: Option<ProviderRole>) -> String {
    ProviderRole::ALL
        .into_iter()
        .filter(|role| filter.is_none_or(|wanted| wanted == *role))
        .map(|role| explain_role(role, pairs))
        .collect::<Vec<_>>()
        .join("\n")
}

fn explain_role(role: ProviderRole, pairs: &[(String, String)]) -> String {
    let Some((_, json)) = pairs.iter().find(|(name, _)| name == role.as_str()) else {
        return format!("{role}: not reported by Providers1.Report.");
    };
    match serde_json::from_str::<Resolution>(json) {
        Ok(resolution) => explain(role, &resolution),
        Err(error) => format!("{role}: could not parse resolution ({error}): {json}"),
    }
}

/// Parse every `(role, resolution_json)` pair `Report()` returned into typed `(role,
/// [`Resolution`])` pairs, dropping entries that name an unknown role or fail to parse (a role
/// the settings page doesn't know yet is a `lege-types` skew problem, not something to crash
/// over). Used by the background page to answer "is this instance a role's provider?" without a
/// second D-Bus round trip.
pub(crate) fn parse_report(pairs: &[(String, String)]) -> Vec<(ProviderRole, Resolution)> {
    pairs
        .iter()
        .filter_map(|(role_name, json)| {
            let role = ProviderRole::ALL
                .into_iter()
                .find(|role| role.as_str() == role_name)?;
            let resolution = serde_json::from_str::<Resolution>(json).ok()?;
            Some((role, resolution))
        })
        .collect()
}

/// Connect to the session bus and render the page. The one impure entry point — everything it
/// delegates to is pure or trait-mediated and covered by tests.
pub(crate) async fn render(connection: &zbus::Connection, role: Option<ProviderRole>) -> String {
    match lege_client::proxies::ProvidersProxy::new(connection).await {
        Ok(proxy) => render_with(&proxy, role).await,
        Err(error) => format!("service unavailable: could not reach Providers1 ({error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> AppId {
        id.parse().unwrap()
    }

    fn row(app_id: &str, name: &str) -> lege_client::proxies::ApplicationRow {
        (
            app_id.to_owned(),
            name.to_owned(),
            String::new(),
            String::new(),
            Vec::new(),
            Vec::new(),
            false,
        )
    }

    /// The chooser reads as the application's name, and keeps the id only where the name alone
    /// would not tell two rows apart. The File Manager row is the one that sent this lane: at the
    /// field's width the id ate the whole label.
    #[test]
    fn a_choice_reads_as_the_application_name_unless_two_of_them_share_it() {
        let names = crate::names::AppNames::from_rows(&[
            row(
                "desktop-import.pcmanfm-qt.desktop",
                "PCManFM-Qt File Manager",
            ),
            row("desktop-import.okular.desktop", "Okular"),
            row("flatpak.org.kde.okular.desktop", "Okular"),
        ]);

        let one = [
            ChoiceOption::Automatic,
            ChoiceOption::App {
                app_id: "desktop-import.pcmanfm-qt.desktop".to_owned(),
                priority: 50,
            },
            // Nothing known about this one: the id is all there is to say.
            ChoiceOption::App {
                app_id: "org.example.Other".to_owned(),
                priority: 10,
            },
        ];
        assert_eq!(
            ChoiceOption::labels(&one, &names),
            [
                "Choose automatically",
                "PCManFM-Qt File Manager · priority 50",
                "org.example.Other · priority 10",
            ]
        );

        // Two copies of one application: without the id the two rows are the same row.
        let two = [
            ChoiceOption::App {
                app_id: "desktop-import.okular.desktop".to_owned(),
                priority: 50,
            },
            ChoiceOption::App {
                app_id: "flatpak.org.kde.okular.desktop".to_owned(),
                priority: 40,
            },
        ];
        assert_eq!(
            ChoiceOption::labels(&two, &names),
            [
                "Okular (desktop-import.okular.desktop) · priority 50",
                "Okular (flatpak.org.kde.okular.desktop) · priority 40",
            ]
        );
    }

    #[test]
    fn selected_is_a_plain_sentence() {
        let resolution = Resolution::Selected {
            app_id: app("org.legeos.NetworkTray"),
        };
        assert_eq!(
            explain(ProviderRole::NetworkStatusUi, &resolution),
            "org.legeos.NetworkTray is selected for network-status-ui."
        );
    }

    #[test]
    fn defaulted_by_priority_names_the_competitors_without_inventing_priorities() {
        let resolution = Resolution::DefaultedByPriority {
            app_id: app("org.legeos.NetworkTray"),
            competitors: vec![app("org.gnome.NmApplet")],
        };
        let sentence = explain(ProviderRole::NetworkStatusUi, &resolution);
        assert_eq!(
            sentence,
            "org.legeos.NetworkTray is the default for network-status-ui by priority, ahead of org.gnome.NmApplet."
        );
        assert!(
            !sentence.contains("priority 100"),
            "no priority number is available to report"
        );
    }

    #[test]
    fn defaulted_by_priority_with_no_competitors_says_so() {
        let resolution = Resolution::DefaultedByPriority {
            app_id: app("org.legeos.NetworkTray"),
            competitors: vec![],
        };
        assert_eq!(
            explain(ProviderRole::NetworkStatusUi, &resolution),
            "org.legeos.NetworkTray is the default for network-status-ui by priority (no other application declares it)."
        );
    }

    #[test]
    fn selection_stale_says_it_no_longer_declares_the_role() {
        let resolution = Resolution::SelectionStale {
            app_id: app("org.legeos.OldTray"),
        };
        assert_eq!(
            explain(ProviderRole::NetworkStatusUi, &resolution),
            "org.legeos.OldTray was selected for network-status-ui, but no longer declares it (uninstalled, or its declaration was withdrawn)."
        );
    }

    #[test]
    fn no_provider_names_the_role() {
        assert_eq!(
            explain(ProviderRole::PowerManager, &Resolution::NoProvider),
            "No application declares power-manager."
        );
    }

    #[test]
    fn resolution_deserializes_from_the_real_wire_shape() {
        let json = r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#;
        let resolution: Resolution = serde_json::from_str(json).unwrap();
        assert_eq!(
            resolution,
            Resolution::Selected {
                app_id: app("org.legeos.NetworkTray")
            }
        );
    }

    struct FakeProviders(Vec<(String, String)>, Vec<(String, u32)>);

    impl FakeProviders {
        fn new(report: Vec<(String, String)>) -> Self {
            Self(report, Vec::new())
        }
    }

    impl ProvidersSource for FakeProviders {
        async fn report(&self) -> Result<Vec<(String, String)>> {
            Ok(self.0.clone())
        }
        async fn candidates(&self, _role: &str) -> Result<Vec<(String, u32)>> {
            Ok(self.1.clone())
        }
        async fn select(&self, _role: &str, _app_id: &str) -> Result<()> {
            Ok(())
        }
        async fn deselect(&self, _role: &str) -> Result<()> {
            Ok(())
        }
    }

    struct FailingProviders;

    impl ProvidersSource for FailingProviders {
        async fn report(&self) -> Result<Vec<(String, String)>> {
            anyhow::bail!("no reply from Providers1")
        }
        async fn candidates(&self, _role: &str) -> Result<Vec<(String, u32)>> {
            anyhow::bail!("no reply from Providers1")
        }
        async fn select(&self, _role: &str, _app_id: &str) -> Result<()> {
            anyhow::bail!("no reply from Providers1")
        }
        async fn deselect(&self, _role: &str) -> Result<()> {
            anyhow::bail!("no reply from Providers1")
        }
    }

    fn choice_for(resolution: &Resolution, candidates: &[(String, u32)]) -> RoleChoice {
        role_choice(ProviderRole::NetworkStatusUi, resolution, candidates)
    }

    fn tray_and_applet() -> Vec<(String, u32)> {
        vec![
            ("org.legeos.NetworkTray".to_owned(), 100),
            ("org.gnome.NmApplet".to_owned(), 50),
        ]
    }

    #[test]
    fn a_role_resolved_by_priority_sits_on_automatic() {
        let choice = choice_for(
            &Resolution::DefaultedByPriority {
                app_id: app("org.legeos.NetworkTray"),
                competitors: vec![app("org.gnome.NmApplet")],
            },
            &tray_and_applet(),
        );
        assert_eq!(choice.selected, 0);
        assert_eq!(choice.options[0], ChoiceOption::Automatic);
        assert_eq!(choice.options.len(), 3);
    }

    #[test]
    fn an_explicit_selection_moves_the_chooser_onto_that_application() {
        let choice = choice_for(
            &Resolution::Selected {
                app_id: app("org.gnome.NmApplet"),
            },
            &tray_and_applet(),
        );
        assert_eq!(
            choice.options[choice.selected],
            ChoiceOption::App {
                app_id: "org.gnome.NmApplet".to_owned(),
                priority: 50
            }
        );
    }

    #[test]
    fn a_stale_selection_falls_back_to_automatic_rather_than_picking_someone_else() {
        // The selected application no longer declares the role, so it is not among the
        // candidates. Landing on whichever application happens to be first would show a pin that
        // does not exist.
        let choice = choice_for(
            &Resolution::SelectionStale {
                app_id: app("org.gone.Tray"),
            },
            &tray_and_applet(),
        );
        assert_eq!(choice.selected, 0);
        assert!(choice.explanation.contains("no longer declares it"));
    }

    #[test]
    fn a_role_nothing_declares_still_offers_automatic() {
        let choice = choice_for(&Resolution::NoProvider, &[]);
        assert_eq!(choice.options, vec![ChoiceOption::Automatic]);
        assert_eq!(choice.selected, 0);
    }

    #[tokio::test]
    async fn every_role_gets_a_row_even_when_candidates_cannot_be_read() {
        let choices = role_choices_with(&FakeProviders(
            vec![(
                "network-status-ui".to_owned(),
                r#"{"kind":"no-provider"}"#.to_owned(),
            )],
            Vec::new(),
        ))
        .await
        .unwrap();
        assert_eq!(choices.len(), ProviderRole::ALL.len());
        assert!(choices.iter().all(|choice| !choice.options.is_empty()));
    }

    #[tokio::test]
    async fn a_report_that_will_not_answer_is_reported_rather_than_shown_as_empty() {
        assert!(
            role_choices_with(&FailingProviders)
                .await
                .unwrap_err()
                .starts_with("service unavailable:")
        );
    }

    #[tokio::test]
    async fn render_with_covers_every_role_and_flags_the_missing_ones() {
        let fake = FakeProviders::new(vec![(
            "network-status-ui".to_owned(),
            r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#.to_owned(),
        )]);
        let rendered = render_with(&fake, None).await;
        assert!(rendered.contains("org.legeos.NetworkTray is selected for network-status-ui."));
        assert!(rendered.contains("power-manager: not reported by Providers1.Report."));
        assert_eq!(rendered.lines().count(), ProviderRole::ALL.len());
    }

    #[tokio::test]
    async fn render_with_can_filter_to_one_role() {
        let fake = FakeProviders::new(vec![]);
        let rendered = render_with(&fake, Some(ProviderRole::Greeter)).await;
        assert_eq!(rendered, "greeter: not reported by Providers1.Report.");
    }

    #[test]
    fn parse_report_drops_unknown_roles_and_bad_json() {
        let pairs = vec![
            (
                "network-status-ui".to_owned(),
                r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#.to_owned(),
            ),
            ("not-a-real-role".to_owned(), "{}".to_owned()),
            ("power-manager".to_owned(), "not json".to_owned()),
        ];
        let parsed = parse_report(&pairs);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, ProviderRole::NetworkStatusUi);
    }

    #[tokio::test]
    async fn render_with_degrades_on_a_report_failure() {
        let rendered = render_with(&FailingProviders, None).await;
        assert!(rendered.starts_with("service unavailable:"));
        assert!(rendered.contains("no reply from Providers1"));
    }
}
