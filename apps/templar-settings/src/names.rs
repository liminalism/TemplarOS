//! Application ids, as a person would recognise them.
//!
//! Every page here deals in `AppId`s, and an `AppId` on a real machine looks like
//! `desktop-import.org.kde.okularApplication_pdf.desktop` — accurate, addressable, and no help at
//! all to someone deciding which application should open their PDFs. `AppIdentity1` already
//! carries the display name from each manifest; one `ListApplications` call for the whole window
//! turns every list on every page into names.
//!
//! The id is never thrown away. It is what the service is told, it is what distinguishes two
//! installed copies of the same application, and a page that showed only "Okular" for both would
//! be asking a person to choose between two identical-looking options.

use anyhow::Context;
use std::collections::HashMap;

/// Display names by application id. An id with no entry keeps its id — a manifest this window
/// could not read is a gap to show, not a row to drop.
#[derive(Debug, Clone, Default)]
pub(crate) struct AppNames {
    names: HashMap<String, String>,
}

impl AppNames {
    pub(crate) fn choices(&self) -> Vec<(String, String)> {
        let mut rows: Vec<_> = self
            .names
            .iter()
            .map(|(id, name)| (id.clone(), name.clone()))
            .collect();
        rows.sort_by(|left, right| {
            left.1
                .to_lowercase()
                .cmp(&right.1.to_lowercase())
                .then_with(|| left.0.cmp(&right.0))
        });
        rows
    }
    /// Build from `AppIdentity1.ListApplications` rows.
    ///
    /// A blank display name is treated as absent: an imported `.desktop` file with no `Name=`
    /// would otherwise render as an empty cell, which reads as a bug rather than as a gap.
    pub(crate) fn from_rows(rows: &[templar_client::proxies::ApplicationRow]) -> Self {
        Self {
            names: rows
                .iter()
                .filter(|row| !row.1.trim().is_empty())
                .map(|row| (row.0.clone(), row.1.clone()))
                .collect(),
        }
    }

    /// The name for `app_id`, falling back to the id itself.
    pub(crate) fn name<'a>(&'a self, app_id: &'a str) -> &'a str {
        self.names.get(app_id).map_or(app_id, String::as_str)
    }

    /// The name, with the id after it when the two differ.
    ///
    /// Used wherever a person is choosing between installed copies: `desktop-import.` and
    /// `flatpak.` prefixes are exactly what distinguishes two rows that would otherwise both say
    /// "Okular", so the name alone would make the choice unmakeable.
    pub(crate) fn qualified(&self, app_id: &str) -> String {
        let name = self.name(app_id);
        if name == app_id {
            app_id.to_owned()
        } else {
            format!("{name} ({app_id})")
        }
    }
}

/// Read every installed application's name.
///
/// A failure yields an empty map rather than an error: every page degrades to showing ids, which
/// is what it showed before this existed, and a settings window that refuses to open because it
/// could not prettify a label would be a worse trade than any label.
pub(crate) async fn load(connection: &zbus::Connection) -> AppNames {
    async {
        let proxy = templar_client::proxies::AppIdentityProxy::new(connection)
            .await
            .context("AppIdentity1")?;
        let rows = proxy
            .list_applications()
            .await
            .context("AppIdentity1.ListApplications")?;
        Ok::<_, anyhow::Error>(AppNames::from_rows(&rows))
    }
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(app_id: &str, name: &str) -> templar_client::proxies::ApplicationRow {
        (
            app_id.to_owned(),
            name.to_owned(),
            format!("{app_id}.desktop"),
            String::new(),
            Vec::new(),
            Vec::new(),
            false,
        )
    }

    #[test]
    fn a_known_id_reads_as_its_name() {
        let names = AppNames::from_rows(&[row("desktop-import.okular.desktop", "Okular")]);
        assert_eq!(names.name("desktop-import.okular.desktop"), "Okular");
    }

    #[test]
    fn an_unknown_id_keeps_its_id_rather_than_going_blank() {
        let names = AppNames::default();
        assert_eq!(names.name("org.example.Ghost"), "org.example.Ghost");
        assert_eq!(names.qualified("org.example.Ghost"), "org.example.Ghost");
    }

    #[test]
    fn a_manifest_with_no_name_is_a_gap_not_an_empty_label() {
        let names = AppNames::from_rows(&[row("desktop-import.mystery.desktop", "   ")]);
        assert_eq!(
            names.name("desktop-import.mystery.desktop"),
            "desktop-import.mystery.desktop"
        );
    }

    #[test]
    fn two_copies_of_one_application_stay_distinguishable() {
        let names = AppNames::from_rows(&[
            row("desktop-import.okular.desktop", "Okular"),
            row("flatpak.org.kde.okular", "Okular"),
        ]);
        assert_ne!(
            names.qualified("desktop-import.okular.desktop"),
            names.qualified("flatpak.org.kde.okular")
        );
    }
}
