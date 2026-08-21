//! Associations page (`org.legeos.Associations1`): which application opens a file or URI, and
//! why — plus the choice a person would be making if they changed it.
//!
//! `docs/architecture/associations.md`: "Choice UI distinguishes open once / always for MIME /
//! always for extension / always in this project / ask when extension and content disagree."
//! There is no interactive picker here (no toolkit in this milestone), so [`ChoiceScope`] models
//! that distinction as a type rather than a UI — but the *explanation* of what is currently
//! happening always comes from the real `ResolveFile`/`ResolveUri`/`Explain` calls, never
//! invented text: `Explain` already returns the exact human sentence
//! (`crates/lege-associationd/src/dbus.rs::explain_decision`), so this page prints it verbatim.

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
/// (`crates/lege-associationd/src/engine.rs`), as returned by `ResolveFile`/`ResolveUri`'s
/// `reason` output and documented in `interfaces/dbus/org.legeos.Associations1.xml`.
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
    async fn resolve_file(&self, path: &str, project_id: &str) -> Result<(String, String, String)>;
    async fn resolve_uri(&self, uri: &str, project_id: &str) -> Result<(String, String, String)>;
    async fn explain(&self, path: &str, project_id: &str) -> Result<String>;
}

impl AssociationsSource for lege_client::proxies::AssociationsProxy<'_> {
    async fn list_mime_defaults(&self) -> Result<Vec<(String, String)>> {
        self.list_mime_defaults()
            .await
            .context("Associations1.ListMimeDefaults")
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
    match lege_client::proxies::AssociationsProxy::new(connection).await {
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

    struct FakeAssociations {
        mime_defaults: Vec<(String, String)>,
        resolve: (String, String, String),
        explanation: String,
    }

    impl AssociationsSource for FakeAssociations {
        async fn list_mime_defaults(&self) -> Result<Vec<(String, String)>> {
            Ok(self.mime_defaults.clone())
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

    #[tokio::test]
    async fn render_with_lists_every_choice_scope() {
        let fake = FakeAssociations {
            mime_defaults: vec![],
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
            mime_defaults: vec![("application/pdf".to_owned(), "org.legeos.Reader".to_owned())],
            resolve: (
                "org.legeos.Reader".to_owned(),
                "application/pdf".to_owned(),
                "global-mime-default".to_owned(),
            ),
            explanation: "report.pdf (detected as application/pdf) opens with org.legeos.Reader: the default application for application/pdf.".to_owned(),
        };
        let rendered = render_with(&fake, Some("report.pdf"), "").await;
        assert!(rendered.contains("application/pdf -> org.legeos.Reader"));
        assert!(rendered.contains("currently governed by: always for this MIME type"));
        assert!(rendered.contains(&fake.explanation));
    }

    #[tokio::test]
    async fn render_with_shows_ask_when_nothing_resolves() {
        let fake = FakeAssociations {
            mime_defaults: vec![],
            resolve: (String::new(), "application/octet-stream".to_owned(), "ask-user".to_owned()),
            explanation: "mystery.bin (detected as application/octet-stream) has no handler set; you will be asked.".to_owned(),
        };
        let rendered = render_with(&fake, Some("mystery.bin"), "").await;
        assert!(rendered.contains("application: (ask)"));
        assert!(rendered.contains("no default governs this yet"));
    }
}
