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
    clients::AssociationsClient,
    request::{owned_value, response, with_request},
};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// What the human would be asked, if anyone could ask them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppChoiceRequest {
    pub app_id: String,
    pub choices: Vec<String>,
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
        tokio::task::spawn_blocking(move || {
            crate::dialog::choice("Open with".into(), message, request.choices)
        })
        .await
        .ok()
        .flatten()
    }
}

fn get_string(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
        .filter(|value| !value.is_empty())
}

/// Whether `resolved` is a usable, confident answer that the frontend actually offered.
fn confident_and_offered(resolved: &str, choices: &[String]) -> bool {
    !resolved.is_empty() && choices.iter().any(|choice| choice == resolved)
}

/// `org.freedesktop.impl.portal.AppChooser`.
#[derive(Debug)]
pub struct AppChooserApi {
    pub associations: Arc<dyn AssociationsClient>,
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

        let automatic = self
            .resolve_automatically(uri.as_deref(), content_type.as_deref())
            .await
            .filter(|resolved| confident_and_offered(resolved, &choices));

        let chosen = match automatic {
            Some(choice) => Some(choice),
            None => {
                self.ui
                    .choose(AppChoiceRequest {
                        app_id: app_id.to_owned(),
                        choices,
                        content_type,
                        uri,
                    })
                    .await
            }
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
                "org.legeos.Viewer".into(),
                "application/pdf".into(),
                "global-mime-default".into(),
            ),
        );
        let api = AppChooserApi {
            associations: Arc::new(associations),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .choose_application(
                "org.legeos.Portal",
                vec!["org.legeos.Viewer".into(), "org.example.Other".into()],
                &options(&[("uri", "file:///a.pdf")]),
            )
            .await;
        assert_eq!(code, response::SUCCESS);
        assert_eq!(
            String::try_from(results["choice"].clone()).unwrap(),
            "org.legeos.Viewer"
        );
    }

    #[tokio::test]
    async fn a_resolution_outside_the_offered_choices_is_not_trusted() {
        let mut associations = FakeAssociations::default();
        associations.by_uri.insert(
            "file:///a.pdf".into(),
            (
                "org.legeos.Viewer".into(),
                "application/pdf".into(),
                "x".into(),
            ),
        );
        let api = AppChooserApi {
            associations: Arc::new(associations),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, _) = api
            .choose_application(
                "org.legeos.Portal",
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
            mime_defaults: vec![("application/pdf".into(), "org.legeos.Viewer".into())],
        };
        let api = AppChooserApi {
            associations: Arc::new(associations),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .choose_application(
                "org.legeos.Portal",
                vec!["org.legeos.Viewer".into()],
                &options(&[("content_type", "application/pdf")]),
            )
            .await;
        assert_eq!(code, response::SUCCESS);
        assert_eq!(
            String::try_from(results["choice"].clone()).unwrap(),
            "org.legeos.Viewer"
        );
    }

    #[tokio::test]
    async fn nothing_resolves_and_no_ui_is_available_cancels() {
        let api = AppChooserApi {
            associations: Arc::new(FakeAssociations::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .choose_application(
                "org.legeos.Portal",
                vec!["org.example.Other".into()],
                &HashMap::new(),
            )
            .await;
        assert_eq!(code, response::CANCELLED);
        assert!(results.is_empty());
    }
}
