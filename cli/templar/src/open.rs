//! `templar open` — open a file or URI through the association broker, never around it
//! (`docs/architecture/associations.md`).
//!
//! `--explain` prints the whole resolution chain the broker reports rather than only its answer:
//! the detected type, the rule that won, the broker's own sentence, and what would actually be
//! launched. That is the difference between "it opened in the wrong application" and a bug report
//! somebody can act on.
//!
//! Every D-Bus call goes through [`AssociationsClient`] / [`LauncherClient`] so the rendering
//! below is testable with a fixture, never a live `org.templaros.Session`.

use anyhow::{Context, Result};
use async_trait::async_trait;

/// What `templar open` needs from `org.templaros.Associations1`.
#[async_trait]
pub trait AssociationsClient {
    /// `(canonical_app_id, detected_mime, reason)`
    async fn resolve_file(&self, path: &str, project_id: &str) -> Result<(String, String, String)>;
    /// `(canonical_app_id, detected_mime, reason)`
    async fn resolve_uri(&self, uri: &str, project_id: &str) -> Result<(String, String, String)>;
    async fn explain(&self, path: &str, project_id: &str) -> Result<String>;
}

#[async_trait]
impl AssociationsClient for templar_client::proxies::AssociationsProxy<'_> {
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

/// What `templar open` needs from `org.templaros.Launcher1`.
#[async_trait]
pub trait LauncherClient {
    async fn launch(
        &self,
        app_id: &str,
        arguments: Vec<String>,
        activation_token: &str,
    ) -> Result<String>;
}

#[async_trait]
impl LauncherClient for templar_client::proxies::LauncherProxy<'_> {
    async fn launch(
        &self,
        app_id: &str,
        arguments: Vec<String>,
        activation_token: &str,
    ) -> Result<String> {
        self.launch(app_id, arguments, activation_token)
            .await
            .context("Launcher1.Launch")
    }
}

/// Whether a target names a URI with a scheme, or a filesystem path.
///
/// A Windows-style drive letter is not a scheme, and neither is a bare relative path containing a
/// colon; a scheme must start with a letter and be at least two characters, per RFC 3986. A
/// `file://` URI is deliberately treated as a *URI* here so the broker resolves it through
/// `x-scheme-handler/file` rules if it has any — the broker, not this CLI, decides what that means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetKind {
    Uri,
    Path,
}

pub fn classify_target(target: &str) -> TargetKind {
    let Some((scheme, _rest)) = target.split_once(':') else {
        return TargetKind::Path;
    };
    let valid_scheme = scheme.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if valid_scheme {
        TargetKind::Uri
    } else {
        TargetKind::Path
    }
}

/// The full chain the broker reported for one target.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    pub target: String,
    pub kind: TargetKind,
    /// Empty when the broker's answer is "ask the user".
    pub app_id: String,
    pub detected_mime: String,
    /// One of `project-mime-default | explicit-extension-override | global-mime-default |
    /// generic-extension-fallback | ask-user`.
    pub reason: String,
    /// The broker's own sentence, fetched only for `--explain` (it is a second round trip).
    pub explanation: Option<String>,
}

impl Resolution {
    /// Whether the broker resolved to an application, as opposed to "ask the user".
    pub fn is_resolved(&self) -> bool {
        !self.app_id.is_empty()
    }
}

/// Resolve `target` without launching anything. `explain` adds the broker's `Explain` sentence,
/// which only applies to paths — a scheme has no filename for the broker to explain.
pub async fn resolve<A: AssociationsClient + ?Sized>(
    associations: &A,
    target: &str,
    project_id: &str,
    explain: bool,
) -> Result<Resolution> {
    let kind = classify_target(target);
    let (app_id, detected_mime, reason) = match kind {
        TargetKind::Uri => associations.resolve_uri(target, project_id).await?,
        TargetKind::Path => associations.resolve_file(target, project_id).await?,
    };
    let explanation = if explain && kind == TargetKind::Path {
        Some(associations.explain(target, project_id).await?)
    } else {
        None
    };
    Ok(Resolution {
        target: target.to_owned(),
        kind,
        app_id,
        detected_mime,
        reason,
        explanation,
    })
}

/// One line: what this target opens with, and why.
pub fn render_summary(resolution: &Resolution) -> String {
    format!(
        "{}: {} → {} ({})",
        resolution.target,
        if resolution.detected_mime.is_empty() {
            "unknown type"
        } else {
            &resolution.detected_mime
        },
        if resolution.is_resolved() {
            resolution.app_id.as_str()
        } else {
            "ask"
        },
        resolution.reason,
    )
}

/// The whole chain, for `--explain`: every step the broker went through, in the order it applies
/// them, so the answer can be argued with rather than merely observed.
pub fn render_explanation(resolution: &Resolution) -> String {
    let mut out = vec![
        format!("target:    {}", resolution.target),
        format!(
            "kind:      {}",
            match resolution.kind {
                TargetKind::Uri => "URI (resolved against x-scheme-handler/<scheme>)",
                TargetKind::Path => "path (content detection beats the extension)",
            }
        ),
        format!(
            "detected:  {}",
            if resolution.detected_mime.is_empty() {
                "(none)"
            } else {
                &resolution.detected_mime
            }
        ),
        format!("rule:      {}", describe_reason(&resolution.reason)),
        format!(
            "handler:   {}",
            if resolution.is_resolved() {
                resolution.app_id.as_str()
            } else {
                "(none — the broker would ask)"
            }
        ),
    ];
    if let Some(explanation) = &resolution.explanation
        && !explanation.is_empty()
    {
        out.push(format!("broker:    {explanation}"));
    }
    out.join("\n")
}

/// Expand a `reason` token from the broker into the rule it names. Unknown tokens are passed
/// through verbatim rather than being mapped to a guess: the broker owns this vocabulary, and a
/// CLI that silently renames a new reason is worse than one that shows it raw.
pub fn describe_reason(reason: &str) -> String {
    match reason {
        "project-mime-default" => {
            "the open project's default for this MIME type (highest precedence)".to_owned()
        }
        "explicit-extension-override" => {
            "an explicit \"always for this extension\" override the user created".to_owned()
        }
        "global-mime-default" => "the global default for this MIME type".to_owned(),
        "generic-extension-fallback" => {
            "the extension, used only because detection returned a generic type".to_owned()
        }
        "ask-user" => "no rule matched, so the broker would ask".to_owned(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        file: Option<(String, String, String)>,
        uri: Option<(String, String, String)>,
        explanation: String,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AssociationsClient for Fake {
        async fn resolve_file(
            &self,
            path: &str,
            _project_id: &str,
        ) -> Result<(String, String, String)> {
            self.calls.lock().unwrap().push(format!("file:{path}"));
            Ok(self.file.clone().unwrap_or_default())
        }
        async fn resolve_uri(
            &self,
            uri: &str,
            _project_id: &str,
        ) -> Result<(String, String, String)> {
            self.calls.lock().unwrap().push(format!("uri:{uri}"));
            Ok(self.uri.clone().unwrap_or_default())
        }
        async fn explain(&self, path: &str, _project_id: &str) -> Result<String> {
            self.calls.lock().unwrap().push(format!("explain:{path}"));
            Ok(self.explanation.clone())
        }
    }

    #[test]
    fn schemes_are_told_apart_from_paths() {
        assert_eq!(classify_target("https://example.org"), TargetKind::Uri);
        assert_eq!(classify_target("mailto:a@b.c"), TargetKind::Uri);
        assert_eq!(classify_target("file:///tmp/x.pdf"), TargetKind::Uri);
        assert_eq!(classify_target("/home/u/report.pdf"), TargetKind::Path);
        assert_eq!(classify_target("report.pdf"), TargetKind::Path);
        // A single-letter "scheme" is a drive letter, and a colon in a relative name is not a
        // scheme either.
        assert_eq!(classify_target("C:/Users/x"), TargetKind::Path);
        assert_eq!(classify_target("odd:name.txt"), TargetKind::Uri);
        assert_eq!(classify_target("./a:b"), TargetKind::Path);
    }

    #[tokio::test]
    async fn a_path_resolves_through_resolve_file() {
        let fake = Fake {
            file: Some((
                "org.templaros.Viewer".to_owned(),
                "application/pdf".to_owned(),
                "global-mime-default".to_owned(),
            )),
            ..Fake::default()
        };
        let resolution = resolve(&fake, "/home/u/report.pdf", "", false)
            .await
            .unwrap();
        assert!(resolution.is_resolved());
        assert_eq!(resolution.app_id, "org.templaros.Viewer");
        assert_eq!(
            fake.calls.lock().unwrap().as_slice(),
            ["file:/home/u/report.pdf"]
        );
        assert_eq!(
            render_summary(&resolution),
            "/home/u/report.pdf: application/pdf → org.templaros.Viewer (global-mime-default)"
        );
    }

    #[tokio::test]
    async fn a_uri_resolves_through_resolve_uri_and_is_never_explained() {
        let fake = Fake {
            uri: Some((
                "org.mozilla.Waterfox".to_owned(),
                "x-scheme-handler/https".to_owned(),
                "global-mime-default".to_owned(),
            )),
            explanation: "should not be asked for".to_owned(),
            ..Fake::default()
        };
        // `explain = true`, but a scheme has no filename for the broker to explain, so Explain
        // must not be called at all.
        let resolution = resolve(&fake, "https://example.org", "", true)
            .await
            .unwrap();
        assert_eq!(resolution.kind, TargetKind::Uri);
        assert_eq!(resolution.explanation, None);
        assert_eq!(
            fake.calls.lock().unwrap().as_slice(),
            ["uri:https://example.org"]
        );
    }

    #[tokio::test]
    async fn explain_on_a_path_adds_the_brokers_own_sentence() {
        let fake = Fake {
            file: Some((
                "org.templaros.Viewer".to_owned(),
                "application/pdf".to_owned(),
                "explicit-extension-override".to_owned(),
            )),
            explanation: "You chose Templar Viewer for .pdf files on 3 August.".to_owned(),
            ..Fake::default()
        };
        let resolution = resolve(&fake, "/home/u/report.pdf", "", true)
            .await
            .unwrap();
        let rendered = render_explanation(&resolution);
        assert!(rendered.contains("application/pdf"));
        assert!(rendered.contains("always for this extension"));
        assert!(rendered.contains("You chose Templar Viewer"));
        assert!(rendered.contains("org.templaros.Viewer"));
    }

    #[tokio::test]
    async fn an_unresolved_target_reads_as_ask_rather_than_an_empty_app_id() {
        let fake = Fake {
            file: Some((
                String::new(),
                "application/octet-stream".to_owned(),
                "ask-user".to_owned(),
            )),
            ..Fake::default()
        };
        let resolution = resolve(&fake, "/home/u/mystery.bin", "", false)
            .await
            .unwrap();
        assert!(!resolution.is_resolved());
        assert!(render_summary(&resolution).contains("→ ask"));
        assert!(render_explanation(&resolution).contains("the broker would ask"));
    }

    #[test]
    fn an_unknown_reason_is_passed_through_rather_than_guessed_at() {
        assert_eq!(describe_reason("some-future-rule"), "some-future-rule");
    }
}
