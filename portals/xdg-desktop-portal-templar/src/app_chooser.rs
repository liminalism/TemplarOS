//! `org.freedesktop.impl.portal.AppChooser`.
//!
//! The real part is trying automatic resolution through `Associations1` before ever bothering a
//! human: if the caller gave a `uri`, `ResolveUri` may already answer confidently (an empty
//! `canonical_app_id` means "ask" — `docs/protocols/launch-and-activation.md`); with only a bare
//! `content_type`, the global MIME defaults from `ListMimeDefaults` answer the same question. A
//! resolution only counts if it names one of the `choices` the frontend actually offered — this
//! backend must never hand back an application the caller did not list. When nothing resolves,
//! the interactive multi-way chooser is the [`AppChooserUi`] seam, honestly cancelled by
//! [`NoUiAvailable`] until a toolkit exists to show it.

use crate::{
    clients::{AppStateClient, AssociationsClient},
    request::{owned_value, response, with_request},
};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// What the human would be asked, if anyone could ask them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppChoiceRequest {
    /// The choices as the frontend spelled them — one of these is the answer.
    pub choices: Vec<String>,
    /// What to show for each of `choices`, in the same order.
    pub labels: Vec<String>,
    pub app_id: String,
    pub content_type: Option<String>,
    pub uri: Option<String>,
}

#[async_trait]
pub trait AppChooserUi: Send + Sync + std::fmt::Debug {
    async fn choose(&self, request: AppChoiceRequest) -> Option<String>;
}

#[derive(Debug, Default)]
pub struct NoUiAvailable;

#[async_trait]
impl AppChooserUi for NoUiAvailable {
    async fn choose(&self, _request: AppChoiceRequest) -> Option<String> {
        None
    }
}

/// Pixelkit graphical application chooser used by the live portal backend.
#[derive(Debug, Default)]
pub struct NativeUi;

#[async_trait]
impl AppChooserUi for NativeUi {
    async fn choose(&self, request: AppChoiceRequest) -> Option<String> {
        let message = request.content_type.as_deref().map_or_else(
            || "Choose an application".to_owned(),
            |kind| format!("Open {kind} with"),
        );
        // The person picks a name; the frontend is answered with the id it offered for it.
        let choices = request.choices.clone();
        let labels = request.labels.clone();
        let picked = tokio::task::spawn_blocking(move || {
            crate::dialog::choice("Open with".into(), message, labels)
        })
        .await
        .ok()
        .flatten()?;
        request
            .labels
            .iter()
            .position(|label| *label == picked)
            .and_then(|at| choices.get(at).cloned())
    }
}

fn get_string(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
        .filter(|value| !value.is_empty())
}

/// The frontend's own spelling of `resolved`, if it offered that application at all.
///
/// The two sides name applications differently: `org.freedesktop.impl.portal.AppChooser`'s
/// `choices` are desktop file ids (`firefox`, sometimes `firefox.desktop`), while `Associations1`
/// answers with a TemplarOS canonical app id. `applications` is `AppIdentity1.ListApplications`'s
/// `(app_id, display_name, desktop_file_id)`, which is what lets the two meet. The answer is
/// always the string the *caller* used — a backend must hand back a choice the frontend offered,
/// never a name of its own.
fn offered_spelling(
    resolved: &str,
    choices: &[String],
    applications: &[(String, String, String)],
) -> Option<String> {
    if resolved.is_empty() {
        return None;
    }
    let desktop_id = applications
        .iter()
        .find(|(app_id, _, _)| app_id == resolved)
        .map(|(_, _, desktop_file_id)| desktop_file_id.as_str());
    choices
        .iter()
        .find(|choice| {
            let choice = stem(choice);
            choice == stem(resolved) || desktop_id.is_some_and(|id| choice == stem(id))
        })
        .cloned()
}

/// A desktop file id without its `.desktop` suffix — the two spellings mean the same entry.
fn stem(value: &str) -> &str {
    value.strip_suffix(".desktop").unwrap_or(value)
}

/// What to show a person for `choice`, preferring the application's own display name over a
/// desktop file id nobody would recognise.
fn label_for(choice: &str, applications: &[(String, String, String)]) -> String {
    applications
        .iter()
        .find(|(app_id, _, desktop_file_id)| {
            stem(app_id) == stem(choice) || stem(desktop_file_id) == stem(choice)
        })
        .map(|(_, display_name, _)| display_name.clone())
        .filter(|display_name| !display_name.is_empty())
        .unwrap_or_else(|| choice.to_owned())
}

/// `org.freedesktop.impl.portal.AppChooser`.
#[derive(Debug)]
pub struct AppChooserApi {
    pub associations: Arc<dyn AssociationsClient>,
    pub apps: Arc<dyn AppStateClient>,
    pub ui: Arc<dyn AppChooserUi>,
}

impl AppChooserApi {
    async fn resolve_automatically(
        &self,
        uri: Option<&str>,
        content_type: Option<&str>,
    ) -> Option<String> {
        if let Some(uri) = uri {
            let (app_id, _mime, _reason) = self.associations.resolve_uri(uri).await.ok()?;
            if !app_id.is_empty() {
                return Some(app_id);
            }
        }
        if let Some(content_type) = content_type {
            let defaults = self.associations.list_mime_defaults().await.ok()?;
            return defaults
                .into_iter()
                .find(|(mime, _)| mime == content_type)
                .map(|(_, app_id)| app_id);
        }
        None
    }

    async fn choose_application(
        &self,
        app_id: &str,
        choices: Vec<String>,
        options: &HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let uri = get_string(options, "uri");
        let content_type = get_string(options, "content_type");
        let activation_token = get_string(options, "activation_token").unwrap_or_default();

        // One call, used twice: to recognise the resolved default among the offered choices, and
        // to label the chooser with names a person recognises. A failure here only costs us the
        // mapping, never the choice.
        let applications = self.apps.list_applications().await.unwrap_or_default();

        let automatic = self
            .resolve_automatically(uri.as_deref(), content_type.as_deref())
            .await
            .and_then(|resolved| offered_spelling(&resolved, &choices, &applications));

        let chosen = if let Some(choice) = automatic {
            Some(choice)
        } else {
            let labels = choices
                .iter()
                .map(|choice| label_for(choice, &applications))
                .collect();
            self.ui
                .choose(AppChoiceRequest {
                    app_id: app_id.to_owned(),
                    choices: choices.clone(),
                    labels,
                    content_type,
                    uri,
                })
                .await
        };

        match chosen {
            Some(choice) => {
                let mut results = HashMap::new();
                results.insert("choice".to_owned(), owned_value(choice));
                results.insert("activation_token".to_owned(), owned_value(activation_token));
                (response::SUCCESS, results)
            }
            None => (response::CANCELLED, HashMap::new()),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.AppChooser")]
impl AppChooserApi {
    /// `parent_window` is part of the fixed spec signature; this backend needs no
    /// window-parenting (no toolkit dialog exists yet — see `AppChooserUi`).
    #[zbus(name = "ChooseApplication")]
    #[allow(unused_variables)]
    async fn choose_application_method(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        choices: Vec<String>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        with_request(connection, &handle, || {
            self.choose_application(app_id, choices, &options)
        })
        .await
    }

    /// The frontend may narrow the offered choices between the call and the reply. Our methods
    /// have already returned by the time a caller could invoke this (no interactive dialog stays
    /// open), so there is nothing live to update; accepted and logged rather than refused, since
    /// a spec-conforming frontend is allowed to call it regardless.
    #[zbus(name = "UpdateChoices")]
    #[allow(unused_variables)]
    async fn update_choices(&self, handle: OwnedObjectPath, choices: Vec<String>) {
        tracing::debug!(
            ?choices,
            "UpdateChoices received after the request already answered"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct FakeAssociations {
        by_uri: std::collections::HashMap<String, (String, String, String)>,
        mime_defaults: Vec<(String, String)>,
    }

    #[async_trait]
    impl AssociationsClient for FakeAssociations {
        async fn resolve_uri(&self, uri: &str) -> anyhow::Result<(String, String, String)> {
            Ok(self.by_uri.get(uri).cloned().unwrap_or_default())
        }
        async fn list_mime_defaults(&self) -> anyhow::Result<Vec<(String, String)>> {
            Ok(self.mime_defaults.clone())
        }
    }

    /// `AppIdentity1.ListApplications` for the tests: `(app_id, display_name, desktop_file_id)`.
    #[derive(Debug, Default)]
    struct FakeApps(Vec<(String, String, String)>);

    #[async_trait]
    impl AppStateClient for FakeApps {
        async fn list_instances(&self) -> anyhow::Result<Vec<(String, String, String, String)>> {
            Ok(Vec::new())
        }
        async fn report(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn list_applications(&self) -> anyhow::Result<Vec<(String, String, String)>> {
            Ok(self.0.clone())
        }
    }

    fn options(pairs: &[(&str, &str)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), owned_value(*v)))
            .collect()
    }

    #[tokio::test]
    async fn a_confident_uri_resolution_is_answered_without_asking() {
        let mut associations = FakeAssociations::default();
        associations.by_uri.insert(
            "file:///a.pdf".into(),
            (
                "org.templaros.Viewer".into(),
                "application/pdf".into(),
                "global-mime-default".into(),
            ),
        );
        let api = AppChooserApi {
            associations: Arc::new(associations),
            apps: Arc::new(FakeApps::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .choose_application(
                "org.templaros.Portal",
                vec!["org.templaros.Viewer".into(), "org.example.Other".into()],
                &options(&[("uri", "file:///a.pdf")]),
            )
            .await;
        assert_eq!(code, response::SUCCESS);
        assert_eq!(
            String::try_from(results["choice"].clone()).unwrap(),
            "org.templaros.Viewer"
        );
    }

    #[tokio::test]
    async fn a_resolution_outside_the_offered_choices_is_not_trusted() {
        let mut associations = FakeAssociations::default();
        associations.by_uri.insert(
            "file:///a.pdf".into(),
            (
                "org.templaros.Viewer".into(),
                "application/pdf".into(),
                "x".into(),
            ),
        );
        let api = AppChooserApi {
            associations: Arc::new(associations),
            apps: Arc::new(FakeApps::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, _) = api
            .choose_application(
                "org.templaros.Portal",
                vec!["org.example.Other".into()],
                &options(&[("uri", "file:///a.pdf")]),
            )
            .await;
        assert_eq!(code, response::CANCELLED);
    }

    #[tokio::test]
    async fn a_bare_content_type_falls_back_to_global_mime_defaults() {
        let associations = FakeAssociations {
            by_uri: std::collections::HashMap::new(),
            mime_defaults: vec![("application/pdf".into(), "org.templaros.Viewer".into())],
        };
        let api = AppChooserApi {
            associations: Arc::new(associations),
            apps: Arc::new(FakeApps::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .choose_application(
                "org.templaros.Portal",
                vec!["org.templaros.Viewer".into()],
                &options(&[("content_type", "application/pdf")]),
            )
            .await;
        assert_eq!(code, response::SUCCESS);
        assert_eq!(
            String::try_from(results["choice"].clone()).unwrap(),
            "org.templaros.Viewer"
        );
    }

    #[tokio::test]
    async fn nothing_resolves_and_no_ui_is_available_cancels() {
        let api = AppChooserApi {
            associations: Arc::new(FakeAssociations::default()),
            apps: Arc::new(FakeApps::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .choose_application(
                "org.templaros.Portal",
                vec!["org.example.Other".into()],
                &HashMap::new(),
            )
            .await;
        assert_eq!(code, response::CANCELLED);
        assert!(results.is_empty());
    }

    /// The journey behind "open this downloaded file": `xdg-desktop-portal` implements
    /// `org.freedesktop.portal.OpenURI` itself and delegates only the *which application*
    /// question to this backend — offering desktop file ids, not TemplarOS app ids. Before this
    /// mapping existed the TemplarOS default never matched an offered choice, so a system that knew
    /// the answer asked the person anyway.
    #[tokio::test]
    async fn openuri_resolves_default_handler() {
        let mut associations = FakeAssociations::default();
        associations.by_uri.insert(
            "file:///home/u/Downloads/report.pdf".into(),
            (
                "org.templaros.Viewer".into(),
                "application/pdf".into(),
                "global-mime-default".into(),
            ),
        );
        let api = AppChooserApi {
            associations: Arc::new(associations),
            apps: Arc::new(FakeApps(vec![(
                "org.templaros.Viewer".into(),
                "Document Viewer".into(),
                "org.templaros.Viewer.desktop".into(),
            )])),
            // No dialog: if the default did not resolve, this cancels rather than guessing.
            ui: Arc::new(NoUiAvailable),
        };

        let (code, results) = api
            .choose_application(
                "org.mozilla.firefox",
                vec!["org.templaros.Viewer.desktop".into(), "gimp".into()],
                &options(&[("uri", "file:///home/u/Downloads/report.pdf")]),
            )
            .await;

        assert_eq!(code, response::SUCCESS);
        assert_eq!(
            String::try_from(results["choice"].clone()).unwrap(),
            "org.templaros.Viewer.desktop",
            "answered with the frontend's own spelling of the choice it offered"
        );

        // And when a person must be asked, they are asked with names, not desktop file ids.
        let applications = vec![(
            "org.templaros.Viewer".to_owned(),
            "Document Viewer".to_owned(),
            "org.templaros.Viewer.desktop".to_owned(),
        )];
        assert_eq!(
            label_for("org.templaros.Viewer.desktop", &applications),
            "Document Viewer"
        );
        assert_eq!(label_for("gimp", &applications), "gimp");
    }
}
