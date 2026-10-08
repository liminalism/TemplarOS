//! Notifications section of the System page: the terminal-completion policy, drawn by
//! the ordinary key form in the GUI and listed by `--print` here.
//!
//! The typed keys are `shell.notifications.*` on `Settings1`
//! (`templar_session_core::settings`). A new terminal picks the prefilter threshold up at
//! startup; the completion route itself reads live, so a change here needs no restart.

use anyhow::Result;

/// The key prefix this section owns.
pub(crate) const PREFIX: &str = "shell.notifications.";

/// The section's keys at their effective values, in key order, out of a raw
/// `Settings1.List` answer. Every other key is another section's business.
pub(crate) fn section_rows(values: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = values
        .into_iter()
        .filter(|(key, _)| key.starts_with(PREFIX))
        .collect();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows
}

/// One `key = value` line per row, the way the other prefix pages print.
pub(crate) fn format_section(rows: &[(String, String)]) -> String {
    rows.iter()
        .map(|(key, value)| format!("{key} = {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn print() -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = templar_client::proxies::SettingsProxy::new(&connection).await?;
    println!("{}", format_section(&section_rows(proxy.list().await?)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_sections_keys_print_in_key_order() {
        let values = vec![
            ("shell.rails.mode".to_owned(), "x".to_owned()),
            (
                "shell.notifications.notify-every-job".to_owned(),
                "{\"type\":\"bool\",\"value\":false}".to_owned(),
            ),
            (
                "shell.notifications.long-job-seconds".to_owned(),
                "{\"type\":\"int\",\"value\":10}".to_owned(),
            ),
        ];
        let rows = section_rows(values);
        assert_eq!(
            rows.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            vec![
                "shell.notifications.long-job-seconds",
                "shell.notifications.notify-every-job",
            ]
        );
        assert_eq!(
            format_section(&rows),
            "shell.notifications.long-job-seconds = {\"type\":\"int\",\"value\":10}\n\
             shell.notifications.notify-every-job = {\"type\":\"bool\",\"value\":false}"
        );
        assert!(section_rows(Vec::new()).is_empty());
        assert_eq!(format_section(&[]), "");
    }
}
