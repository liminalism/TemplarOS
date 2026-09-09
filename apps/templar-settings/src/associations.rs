//! Associations page (`org.templaros.Associations1`): which application opens a file or URI, and
//! why — plus the choice a person would be making if they changed it.
//!
//! `docs/architecture/associations.md`: "Choice UI distinguishes open once / always for MIME /
//! always for extension / always in this project / ask when extension and content disagree."
//! [`ChoiceScope`] models that distinction as a type. The graphical page offers the MIME half of
//! it — the one scope that stands on its own, without a file in hand to name an extension or a
//! project — and the `--print` renderer still lists all five. The *explanation* of what is
//! currently happening always comes from the real `ResolveFile`/`ResolveUri`/`Explain` calls,
//! never invented text: `Explain` already returns the exact human sentence
//! (`crates/templar-associationd/src/dbus.rs::explain_decision`), so this page prints it verbatim.

use anyhow::{Context, Result};

/// The five ways a person can scope a default when they change one (planning doc, associations
/// architecture note). A model of the choice UI's shape, independent of any one resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChoiceScope {
    OpenOnce,
    AlwaysForThisMime,
    AlwaysForThisExtension,
    AlwaysInThisProject,
    AskWhenExtensionAndContentDisagree,
}

impl ChoiceScope {
    pub(crate) const ALL: [Self; 5] = [
        Self::OpenOnce,
        Self::AlwaysForThisMime,
        Self::AlwaysForThisExtension,
        Self::AlwaysInThisProject,
        Self::AskWhenExtensionAndContentDisagree,
    ];

    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::OpenOnce => "open once",
            Self::AlwaysForThisMime => "always for this MIME type",
            Self::AlwaysForThisExtension => "always for this extension",
            Self::AlwaysInThisProject => "always in this project",
            Self::AskWhenExtensionAndContentDisagree => {
                "ask when the extension and the detected content disagree"
            }
        }
    }
}

/// Mirrors `crate::engine::DecisionReason`'s wire strings
/// (`crates/templar-associationd/src/engine.rs`), as returned by `ResolveFile`/`ResolveUri`'s
/// `reason` output and documented in `interfaces/dbus/org.templaros.Associations1.xml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveReason {
    ProjectMimeDefault,
    ExplicitExtensionOverride,
    GlobalMimeDefault,
    GenericExtensionFallback,
    AskUser,
}

impl ResolveReason {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "project-mime-default" => Self::ProjectMimeDefault,
            "explicit-extension-override" => Self::ExplicitExtensionOverride,
            "global-mime-default" => Self::GlobalMimeDefault,
            "generic-extension-fallback" => Self::GenericExtensionFallback,
            "ask-user" => Self::AskUser,
            _ => return None,
        })
    }

    /// Which [`ChoiceScope`] is currently in force for this resolution, if any. `AskUser` means
    /// nothing is set yet, so there is nothing "governing" to name — the person is choosing one
    /// of [`ChoiceScope::ALL`] for the first time, not overriding an existing one.
    pub(crate) fn governing_scope(self) -> Option<ChoiceScope> {
        match self {
            Self::ProjectMimeDefault => Some(ChoiceScope::AlwaysInThisProject),
            Self::ExplicitExtensionOverride | Self::GenericExtensionFallback => {
                Some(ChoiceScope::AlwaysForThisExtension)
            }
            Self::GlobalMimeDefault => Some(ChoiceScope::AlwaysForThisMime),
            Self::AskUser => None,
        }
    }
}

/// The bus operations this page needs.
pub(crate) trait AssociationsSource {
    async fn list_mime_defaults(&self) -> Result<Vec<(String, String)>>;
    async fn handlers(&self, mime: &str) -> Result<Vec<String>>;
    async fn set_mime_default(&self, mime: &str, app_id: &str) -> Result<()>;
    async fn forget(&self, scope: &str, key: &str, project_id: &str) -> Result<bool>;
    async fn resolve_file(&self, path: &str, project_id: &str) -> Result<(String, String, String)>;
    async fn resolve_uri(&self, uri: &str, project_id: &str) -> Result<(String, String, String)>;
    async fn explain(&self, path: &str, project_id: &str) -> Result<String>;
}

impl AssociationsSource for templar_client::proxies::AssociationsProxy<'_> {
    async fn list_mime_defaults(&self) -> Result<Vec<(String, String)>> {
        self.list_mime_defaults()
            .await
            .context("Associations1.ListMimeDefaults")
    }

    async fn handlers(&self, mime: &str) -> Result<Vec<String>> {
        self.handlers(mime).await.context("Associations1.Handlers")
    }

    async fn set_mime_default(&self, mime: &str, app_id: &str) -> Result<()> {
        self.set_mime_default(mime, app_id)
            .await
            .context("Associations1.SetMimeDefault")
    }

    async fn forget(&self, scope: &str, key: &str, project_id: &str) -> Result<bool> {
        self.forget(scope, key, project_id)
            .await
            .context("Associations1.Forget")
    }

    async fn resolve_file(&self, path: &str, project_id: &str) -> Result<(String, String, String)> {
        self.resolve_file(path, project_id)
            .await
            .context("Associations1.ResolveFile")
    }

    async fn resolve_uri(&self, uri: &str, project_id: &str) -> Result<(String, String, String)> {
        self.resolve_uri(uri, project_id)
            .await
            .context("Associations1.ResolveUri")
    }

    async fn explain(&self, path: &str, project_id: &str) -> Result<String> {
        self.explain(path, project_id)
            .await
            .context("Associations1.Explain")
    }
}

/// Whether `target` names a URI (has a `scheme:`) or a plain file path — `ResolveUri` has no
/// extension fallback (a scheme has no filename), so the two need different calls.
fn looks_like_uri(target: &str) -> bool {
    target.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    })
}

/// One MIME type as the graphical page shows it: what opens it now, and everything that could.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MimeChoice {
    pub(crate) mime: String,
    /// The applications offered, in the order they are offered. Index 0 is always
    /// [`ASK_EACH_TIME`], which maps to `Forget` rather than to any application.
    pub(crate) options: Vec<String>,
    /// Which option is in force. Always a valid index into `options`.
    pub(crate) selected: usize,
}

/// The first option of every chooser: no default at all.
///
/// It is not an application, and it is not "the system default" either — `Forget` removes the
/// stored choice, after which the engine resolves by extension rule or asks. Naming it after what
/// a person then experiences is more honest than naming it after the call that produces it.
pub(crate) const ASK_EACH_TIME: &str = "Ask each time";

/// Build one MIME type's chooser from its current default and the handlers the engine will
/// accept.
///
/// A default naming an application that is no longer among the handlers — uninstalled, or its
/// manifest stopped declaring the type — is kept in the list and kept selected. Dropping it would
/// show the chooser sitting on "Ask each time" while the stored default is still there, and the
/// person would have no way to see, or clear, the thing that is actually set.
pub(crate) fn mime_choice(mime: &str, current: Option<&str>, handlers: &[String]) -> MimeChoice {
    let mut options = vec![ASK_EACH_TIME.to_owned()];
    options.extend(handlers.iter().cloned());
    if let Some(current) = current
        && !options.iter().any(|option| option == current)
    {
        options.push(current.to_owned());
    }
    let selected = current
        .and_then(|current| options.iter().position(|option| option == current))
        .unwrap_or(0);
    MimeChoice {
        mime: mime.to_owned(),
        options,
        selected,
    }
}

/// Every MIME type with a default set, in name order, each with its chooser.
///
/// Only types that *have* a default: the full list of MIME types a machine knows runs to
/// thousands, and a settings page is where a person revisits a choice they made, not where they
/// browse the shared MIME database. Setting a default for a type that has none happens where the
/// file is — the "open with" choice — which is what [`ChoiceScope`] models.
///
/// `with_handlers` decides whether each row's alternatives are fetched. They cost one call per
/// type, and a real machine has around a hundred and fifty defaults imported from `.desktop`
/// files, so doing it while the window is still being built delays every other page for a page
/// nobody has asked for yet. Pass `false` for the first load — the rows are complete and correct
/// without it, each offering the default it has and a way to clear it — and `true` when the page
/// is opened, which is when the alternatives are what a person came for.
pub(crate) async fn mime_choices_with<S: AssociationsSource>(
    source: &S,
    with_handlers: bool,
) -> Result<Vec<MimeChoice>, String> {
    let mut defaults = match source.list_mime_defaults().await {
        Ok(defaults) => defaults,
        Err(error) => return Err(format!("service unavailable: {error:#}")),
    };
    defaults.sort();
    let mut choices = Vec::with_capacity(defaults.len());
    for (mime, app_id) in defaults {
        let handlers = if with_handlers {
            source.handlers(&mime).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        choices.push(mime_choice(&mime, Some(&app_id), &handlers));
    }
    Ok(choices)
}

pub(crate) async fn render_with<S: AssociationsSource>(
    source: &S,
    target: Option<&str>,
    project_id: &str,
) -> String {
    let mut sections = vec![render_choice_scopes(), render_mime_defaults(source).await];
    if let Some(target) = target {
        sections.push(render_resolution(source, target, project_id).await);
    }
    sections.join("\n\n")
}

fn render_choice_scopes() -> String {
    let mut lines = vec!["Changing a default means choosing one of:".to_owned()];
    lines.extend(
        ChoiceScope::ALL
            .into_iter()
            .map(|scope| format!("  - {}", scope.describe())),
    );
    lines.join("\n")
}

async fn render_mime_defaults<S: AssociationsSource>(source: &S) -> String {
    match source.list_mime_defaults().await {
        Ok(defaults) if defaults.is_empty() => "Global MIME defaults: none set.".to_owned(),
        Ok(mut defaults) => {
            defaults.sort();
            let mut lines = vec!["Global MIME defaults:".to_owned()];
            lines.extend(
                defaults
                    .into_iter()
                    .map(|(mime, app_id)| format!("  {mime} -> {app_id}")),
            );
            lines.join("\n")
        }
        Err(error) => format!("service unavailable: {error:#}"),
    }
}

async fn render_resolution<S: AssociationsSource>(
    source: &S,
    target: &str,
    project_id: &str,
) -> String {
    let resolve = if looks_like_uri(target) {
        source.resolve_uri(target, project_id).await
    } else {
        source.resolve_file(target, project_id).await
    };
    let (app_id, mime, reason) = match resolve {
        Ok(resolved) => resolved,
        Err(error) => return format!("service unavailable: {error:#}"),
    };

    let who = if app_id.is_empty() {
        "(ask)"
    } else {
        app_id.as_str()
    };
    let scope = ResolveReason::parse(&reason)
        .and_then(ResolveReason::governing_scope)
        .map_or_else(
            || "no default governs this yet".to_owned(),
            |scope| format!("currently governed by: {}", scope.describe()),
        );

    let explanation = match source.explain(target, project_id).await {
        Ok(sentence) => sentence,
        Err(error) => format!("(could not fetch Explain: {error:#})"),
    };

    format!(
        "Resolution for {target}:\n  application: {who}\n  detected MIME: {mime}\n  reason: {reason} ({scope})\n  {explanation}"
    )
}

pub(crate) async fn render(
    connection: &zbus::Connection,
    target: Option<&str>,
    project_id: &str,
) -> String {
    match templar_client::proxies::AssociationsProxy::new(connection).await {
        Ok(proxy) => render_with(&proxy, target, project_id).await,
        Err(error) => format!("service unavailable: could not reach Associations1 ({error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_string_parses() {
        for (text, expected) in [
            ("project-mime-default", ResolveReason::ProjectMimeDefault),
            (
                "explicit-extension-override",
                ResolveReason::ExplicitExtensionOverride,
            ),
            ("global-mime-default", ResolveReason::GlobalMimeDefault),
            (
                "generic-extension-fallback",
                ResolveReason::GenericExtensionFallback,
            ),
            ("ask-user", ResolveReason::AskUser),
        ] {
            assert_eq!(ResolveReason::parse(text), Some(expected));
        }
        assert_eq!(ResolveReason::parse("nonsense"), None);
    }

    #[test]
    fn ask_user_governs_nothing() {
        assert_eq!(ResolveReason::AskUser.governing_scope(), None);
    }

    #[test]
    fn project_default_governs_the_project_scope() {
        assert_eq!(
            ResolveReason::ProjectMimeDefault.governing_scope(),
            Some(ChoiceScope::AlwaysInThisProject)
        );
    }

    #[test]
    fn uri_detection_distinguishes_schemes_from_windows_paths_and_bare_names() {
        assert!(looks_like_uri("https://example.com/x"));
        assert!(looks_like_uri("mailto:person@example.com"));
        assert!(!looks_like_uri("/home/user/file.pdf"));
        assert!(!looks_like_uri("report.pdf"));
    }

    #[derive(Default)]
    struct FakeAssociations {
        mime_defaults: Vec<(String, String)>,
        handlers: Vec<String>,
        resolve: (String, String, String),
        explanation: String,
    }

    impl AssociationsSource for FakeAssociations {
        async fn list_mime_defaults(&self) -> Result<Vec<(String, String)>> {
            Ok(self.mime_defaults.clone())
        }

        async fn handlers(&self, _mime: &str) -> Result<Vec<String>> {
            Ok(self.handlers.clone())
        }

        async fn set_mime_default(&self, _mime: &str, _app_id: &str) -> Result<()> {
            Ok(())
        }

        async fn forget(&self, _scope: &str, _key: &str, _project_id: &str) -> Result<bool> {
            Ok(true)
        }

        async fn resolve_file(
            &self,
            _path: &str,
            _project_id: &str,
        ) -> Result<(String, String, String)> {
            Ok(self.resolve.clone())
        }

        async fn resolve_uri(
            &self,
            _uri: &str,
            _project_id: &str,
        ) -> Result<(String, String, String)> {
            Ok(self.resolve.clone())
        }

        async fn explain(&self, _path: &str, _project_id: &str) -> Result<String> {
            Ok(self.explanation.clone())
        }
    }

    fn readers() -> Vec<String> {
        vec![
            "org.templaros.Reader".to_owned(),
            "org.mozilla.firefox".to_owned(),
        ]
    }

    #[test]
    fn a_chooser_offers_ask_each_time_first_and_the_handlers_after_it() {
        let choice = mime_choice("application/pdf", Some("org.templaros.Reader"), &readers());
        assert_eq!(choice.options[0], ASK_EACH_TIME);
        assert_eq!(choice.options.len(), 3);
        assert_eq!(choice.options[choice.selected], "org.templaros.Reader");
    }

    #[test]
    fn a_type_with_no_default_sits_on_ask_each_time() {
        let choice = mime_choice("application/pdf", None, &readers());
        assert_eq!(choice.selected, 0);
    }

    #[test]
    fn a_default_naming_an_application_that_no_longer_handles_the_type_stays_visible() {
        // Uninstalled, or its manifest stopped declaring the type: it is not among the handlers.
        // Showing "Ask each time" here would hide a default that is really set, and leave no way
        // to clear it.
        let choice = mime_choice("application/pdf", Some("org.gone.Reader"), &readers());
        assert_eq!(choice.options[choice.selected], "org.gone.Reader");
        assert!(choice.selected != 0);
    }

    #[test]
    fn a_type_whose_handlers_cannot_be_read_still_offers_what_is_set_and_a_way_out() {
        let choice = mime_choice("application/pdf", Some("org.templaros.Reader"), &[]);
        assert_eq!(choice.options, vec![ASK_EACH_TIME, "org.templaros.Reader"]);
        assert_eq!(choice.selected, 1);
    }

    #[tokio::test]
    async fn mime_choices_are_listed_in_name_order() {
        let choices = mime_choices_with(
            &FakeAssociations {
                mime_defaults: vec![
                    ("text/plain".to_owned(), "org.templaros.Editor".to_owned()),
                    (
                        "application/pdf".to_owned(),
                        "org.templaros.Reader".to_owned(),
                    ),
                ],
                handlers: readers(),
                ..FakeAssociations::default()
            },
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            choices.iter().map(|c| c.mime.as_str()).collect::<Vec<_>>(),
            vec!["application/pdf", "text/plain"]
        );
    }

    #[tokio::test]
    async fn the_first_load_asks_for_the_defaults_and_nothing_else() {
        // Without handlers a row still knows what is set and still offers a way out; what it does
        // not have is the alternatives, which is a call per type the window has not been asked
        // for yet.
        let choices = mime_choices_with(
            &FakeAssociations {
                mime_defaults: vec![(
                    "application/pdf".to_owned(),
                    "org.templaros.Reader".to_owned(),
                )],
                handlers: readers(),
                ..FakeAssociations::default()
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            choices[0].options,
            vec![ASK_EACH_TIME, "org.templaros.Reader"]
        );
        assert_eq!(choices[0].selected, 1);
    }

    #[tokio::test]
    async fn render_with_lists_every_choice_scope() {
        let fake = FakeAssociations {
            mime_defaults: vec![],
            handlers: Vec::new(),
            resolve: (String::new(), String::new(), "ask-user".to_owned()),
            explanation: String::new(),
        };
        let rendered = render_with(&fake, None, "").await;
        for scope in ChoiceScope::ALL {
            assert!(
                rendered.contains(scope.describe()),
                "missing {}",
                scope.describe()
            );
        }
    }

    #[tokio::test]
    async fn render_with_prints_the_real_explain_sentence_verbatim() {
        let fake = FakeAssociations {
            mime_defaults: vec![("application/pdf".to_owned(), "org.templaros.Reader".to_owned())],
            handlers: Vec::new(),
            resolve: (
                "org.templaros.Reader".to_owned(),
                "application/pdf".to_owned(),
                "global-mime-default".to_owned(),
            ),
            explanation: "report.pdf (detected as application/pdf) opens with org.templaros.Reader: the default application for application/pdf.".to_owned(),
        };
        let rendered = render_with(&fake, Some("report.pdf"), "").await;
        assert!(rendered.contains("application/pdf -> org.templaros.Reader"));
        assert!(rendered.contains("currently governed by: always for this MIME type"));
        assert!(rendered.contains(&fake.explanation));
    }

    #[tokio::test]
    async fn render_with_shows_ask_when_nothing_resolves() {
        let fake = FakeAssociations {
            mime_defaults: vec![],
            handlers: Vec::new(),
            resolve: (String::new(), "application/octet-stream".to_owned(), "ask-user".to_owned()),
            explanation: "mystery.bin (detected as application/octet-stream) has no handler set; you will be asked.".to_owned(),
        };
        let rendered = render_with(&fake, Some("mystery.bin"), "").await;
        assert!(rendered.contains("application: (ask)"));
        assert!(rendered.contains("no default governs this yet"));
    }
}
