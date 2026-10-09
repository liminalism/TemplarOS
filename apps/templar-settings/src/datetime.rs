//! Native date/time settings: systemd owns clock/zone/NTP and locale policy; PixelKit only edits.
use anyhow::{Context, Result, bail};
use jiff::{Timestamp, civil::DateTime, tz::TimeZoneDatabase};
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::{ScrollState, TextFieldState, Ui};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum Permission {
    Allowed,
    Authenticate,
    Denied,
    Unavailable(String),
}
impl Permission {
    fn editable(&self) -> bool {
        matches!(self, Self::Allowed | Self::Authenticate)
    }
    fn label(&self) -> &str {
        match self {
            Self::Allowed => "Authorized",
            Self::Authenticate => "Authentication required when applying",
            Self::Denied => "Not permitted by system policy",
            Self::Unavailable(reason) => reason,
        }
    }
}
#[derive(Debug)]
pub struct Clock {
    pub timezone: String,
    pub ntp: bool,
    pub can_ntp: bool,
    pub synchronized: bool,
    pub local: String,
    pub time_permission: Permission,
    pub zone_permission: Permission,
    pub ntp_permission: Permission,
}
#[derive(Debug, Default)]
pub struct DateTimeState {
    pub clock: Option<Clock>,
    pub zones: Vec<String>,
    pub locale: Option<String>,
    pub locales: Vec<String>,
    pub locale_permission: Option<Permission>,
    pub errors: Vec<String>,
    pub message: Option<String>,
}
#[derive(Debug)]
pub enum DateTimeCommand {
    Refresh,
    SetTime(String),
    SetTimezone(String),
    SetNtp(bool),
    SetLocale(String),
}
#[derive(Default)]
pub struct DateTimeController {
    connection: Option<zbus::Connection>,
}
async fn timedate(connection: &zbus::Connection) -> Result<zbus::Proxy<'_>> {
    Ok(zbus::Proxy::new(
        connection,
        "org.freedesktop.timedate1",
        "/org/freedesktop/timedate1",
        "org.freedesktop.timedate1",
    )
    .await?)
}
async fn localed(connection: &zbus::Connection) -> Result<zbus::Proxy<'_>> {
    Ok(zbus::Proxy::new(
        connection,
        "org.freedesktop.locale1",
        "/org/freedesktop/locale1",
        "org.freedesktop.locale1",
    )
    .await?)
}
async fn permission(connection: &zbus::Connection, action: &str) -> Permission {
    let result: Result<(bool, bool, HashMap<String, String>)> = async {
        let proxy = zbus::Proxy::new(
            connection,
            "org.freedesktop.PolicyKit1",
            "/org/freedesktop/PolicyKit1/Authority",
            "org.freedesktop.PolicyKit1.Authority",
        )
        .await?;
        let name = connection
            .unique_name()
            .context("system bus unique name unavailable")?;
        let subject = (
            "system-bus-name",
            HashMap::from([("name", zbus::zvariant::Value::from(name.as_str()))]),
        );
        Ok(proxy
            .call(
                "CheckAuthorization",
                &(subject, action, HashMap::<String, String>::new(), 0u32, ""),
            )
            .await?)
    }
    .await;
    match result {
        Ok((true, _, _)) => Permission::Allowed,
        Ok((false, true, _)) => Permission::Authenticate,
        Ok(_) => Permission::Denied,
        Err(error) => Permission::Unavailable(format!("Permission unavailable: {error:#}")),
    }
}
fn local_microseconds(text: &str, timezone: &str, database: &TimeZoneDatabase) -> Result<i64> {
    let date = DateTime::strptime("%Y-%m-%d %H:%M:%S", text).context("Use YYYY-MM-DD HH:MM:SS")?;
    let zone = database
        .get(timezone)
        .context("System zoneinfo does not contain the current timezone")?;
    // Neither silently move a nonexistent spring time nor guess which autumn occurrence was meant.
    Ok(zone.to_ambiguous_timestamp(date).unambiguous().context("Local time is ambiguous or nonexistent at a daylight-saving transition; choose an unambiguous time")?.as_microsecond())
}
fn time_locale(values: &[String]) -> Option<String> {
    ["LC_ALL=", "LC_TIME=", "LANG="]
        .into_iter()
        .find_map(|prefix| {
            values.iter().find_map(|value| {
                value
                    .strip_prefix(prefix)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            })
        })
}
fn locale_update(values: &[String], locale: &str) -> Vec<String> {
    let mut result: Vec<_> = values
        .iter()
        .filter(|value| !value.starts_with("LC_TIME="))
        .cloned()
        .collect();
    result.push(format!("LC_TIME={locale}"));
    result
}
async fn installed_locales() -> Result<Vec<String>> {
    let output = tokio::process::Command::new("locale")
        .arg("-a")
        .output()
        .await
        .context("Reading installed locale database")?;
    if !output.status.success() {
        bail!(
            "locale -a failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut values: Vec<_> = String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect();
    values.sort();
    values.dedup();
    Ok(values)
}
impl DateTimeController {
    pub async fn run(&mut self, command: DateTimeCommand) -> DateTimeState {
        if self.connection.is_none() {
            match zbus::Connection::system().await {
                Ok(connection) => self.connection = Some(connection),
                Err(error) => {
                    return DateTimeState {
                        errors: vec![format!("System bus unavailable: {error}")],
                        ..Default::default()
                    };
                }
            }
        }
        let connection = self.connection.as_ref().expect("connected above");
        let operation: Result<()> = async {
            match command {
                DateTimeCommand::Refresh => {},
                DateTimeCommand::SetTime(text) => {
                    let proxy = timedate(connection).await?;
                    if proxy.get_property::<bool>("NTP").await? { bail!("Disable automatic time before editing the clock"); }
                    let timezone: String = proxy.get_property("Timezone").await?;
                    let database = TimeZoneDatabase::from_dir("/usr/share/zoneinfo")?;
                    let micros = local_microseconds(&text, &timezone, &database)?;
                    proxy.call::<_, _, ()>("SetTime", &(micros, false, true)).await?;
                },
                DateTimeCommand::SetTimezone(zone) => {
                    let proxy = timedate(connection).await?;
                    let zones: Vec<String> = proxy.call("ListTimezones", &()).await?;
                    if !zones.contains(&zone) { bail!("Select a timezone offered by the system database"); }
                    proxy.call::<_, _, ()>("SetTimezone", &(zone, true)).await?;
                },
                DateTimeCommand::SetNtp(enabled) => { timedate(connection).await?.call::<_, _, ()>("SetNTP", &(enabled, true)).await?; },
                DateTimeCommand::SetLocale(locale) => {
                    if !installed_locales().await?.contains(&locale) { bail!("Select an installed locale"); }
                    let proxy = localed(connection).await?;
                    let values: Vec<String> = proxy.get_property("Locale").await?;
                    if values.iter().any(|value| value.starts_with("LC_ALL=") && value != "LC_ALL=") { bail!("LC_ALL overrides date formatting; change the global locale policy before setting LC_TIME"); }
                    proxy.call::<_, _, ()>("SetLocale", &(locale_update(&values, &locale), true)).await?;
                },
            }
            Ok(())
        }.await;
        // Always show owner readback, including after authorization denial or a partial failure.
        let mut state = self.read(connection).await;
        state.message = operation
            .err()
            .map(|error| format!("Not applied: {error:#}"));
        state
    }
    async fn read(&self, connection: &zbus::Connection) -> DateTimeState {
        let mut state = DateTimeState::default();
        let clock: Result<Clock> = async {
            let proxy = timedate(connection).await?;
            let timezone: String = proxy.get_property("Timezone").await?;
            let micros: u64 = proxy.get_property("TimeUSec").await?;
            let database = TimeZoneDatabase::from_dir("/usr/share/zoneinfo")?;
            let local = Timestamp::from_microsecond(i64::try_from(micros)?)?
                .to_zoned(database.get(&timezone)?)
                .strftime("%Y-%m-%d %H:%M:%S")
                .to_string();
            Ok(Clock {
                timezone,
                local,
                ntp: proxy.get_property("NTP").await?,
                can_ntp: proxy.get_property("CanNTP").await?,
                synchronized: proxy.get_property("NTPSynchronized").await?,
                time_permission: permission(connection, "org.freedesktop.timedate1.set-time").await,
                zone_permission: permission(connection, "org.freedesktop.timedate1.set-timezone")
                    .await,
                ntp_permission: permission(connection, "org.freedesktop.timedate1.set-ntp").await,
            })
        }
        .await;
        match clock {
            Ok(clock) => state.clock = Some(clock),
            Err(error) => state
                .errors
                .push(format!("Date/time unavailable: {error:#}")),
        }
        let zones: Result<Vec<String>> = async {
            Ok(timedate(connection)
                .await?
                .call("ListTimezones", &())
                .await?)
        }
        .await;
        match zones {
            Ok(mut zones) => {
                zones.sort();
                zones.dedup();
                state.zones = zones;
            }
            Err(error) => state
                .errors
                .push(format!("Timezone database unavailable: {error:#}")),
        }
        let locale: Result<Vec<String>> =
            async { Ok(localed(connection).await?.get_property("Locale").await?) }.await;
        match locale {
            Ok(values) => {
                state.locale = Some(time_locale(&values).unwrap_or_else(|| "C".to_owned()));
                state.locale_permission = Some(
                    if values
                        .iter()
                        .any(|value| value.starts_with("LC_ALL=") && value != "LC_ALL=")
                    {
                        Permission::Unavailable(
                            "LC_ALL overrides date formatting; change global locale policy first"
                                .into(),
                        )
                    } else {
                        permission(connection, "org.freedesktop.locale1.set-locale").await
                    },
                );
            }
            Err(error) => state
                .errors
                .push(format!("Global date locale unavailable: {error:#}")),
        }
        match installed_locales().await {
            Ok(locales) => state.locales = locales,
            Err(error) => state
                .errors
                .push(format!("Installed locales unavailable: {error:#}")),
        }
        state
    }
}
#[derive(Default)]
pub struct DateTimeView {
    pub state: DateTimeState,
    time: TextFieldState,
    search: TextFieldState,
    selected_zone: Option<String>,
    selected_locale: Option<String>,
    initialized: bool,
    scroll: ScrollState,
    zones_scroll: ScrollState,
    locales_scroll: ScrollState,
    time_drawn: bool,
    search_drawn: bool,
    summary: String,
    locale_label: String,
    zone_apply_label: String,
    locale_apply_label: String,
    filter: Option<String>,
    filtered_zones: Vec<usize>,
    zone_heights: Vec<i32>,
    locale_heights: Vec<i32>,
}
impl DateTimeView {
    pub fn replace(&mut self, state: DateTimeState) {
        if let Some(clock) = &state.clock {
            self.time.set_text(clock.local.clone());
            self.selected_zone = Some(clock.timezone.clone());
            self.summary = format!(
                "{} · {} · Automatic: {} · Synchronized: {}",
                clock.local, clock.timezone, clock.ntp, clock.synchronized
            );
            self.zone_apply_label = format!("Apply timezone: {}", clock.timezone);
        }
        self.selected_locale = state
            .locale
            .as_ref()
            .filter(|locale| state.locales.contains(locale))
            .cloned();
        self.locale_label = format!(
            "Current date locale: {}",
            state.locale.as_deref().unwrap_or("unavailable")
        );
        self.locale_apply_label = format!(
            "Apply date locale: {}",
            state.locale.as_deref().unwrap_or("unavailable")
        );
        self.locale_heights.resize(state.locales.len(), 0);
        self.filter = None;
        self.state = state;
        self.initialized = true;
    }
    pub fn caret_area(&self) -> Option<Rect> {
        if self.time_drawn {
            self.time.caret_area()
        } else {
            None
        }
        .or_else(|| {
            if self.search_drawn {
                self.search.caret_area()
            } else {
                None
            }
        })
    }
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect) -> Option<DateTimeCommand> {
        self.time_drawn = false;
        self.search_drawn = false;
        let mut command = None;
        let errors = self.state.errors.len() + usize::from(self.state.message.is_some());
        let heights = [ui.px(1100 + i32::try_from(errors).unwrap_or(0) * 66)];
        ui.scroll_list(&mut self.scroll, area, &heights, |ui, _, rect| {
            let h = ui.px(30); let gap = ui.px(6); let mut y = rect.y;
            if ui.button(Rect::new(rect.x, y, ui.px(160), h), "Read system state") { command = Some(DateTimeCommand::Refresh); }
            y += h + gap;
            ui.label(Rect::new(rect.x, y, rect.w, h * 2), "Clock changes use system authorization. Date locale is global; applications adopt it at their next start.", ui.theme.text_dim, Align::Left); y += h * 2 + gap;
            if !self.initialized { ui.label(Rect::new(rect.x, y, rect.w, h), "Reading system date/time…", ui.theme.text_dim, Align::Left); return; }
            for error in self.state.errors.iter().chain(self.state.message.iter()) {
                ui.label(Rect::new(rect.x, y, rect.w, h * 2), error, ui.theme.alarm, Align::Left); y += h * 2 + gap;
            }
            if let Some(clock) = &self.state.clock {
                ui.label(Rect::new(rect.x, y, rect.w, h * 2), &self.summary, ui.theme.text, Align::Left); y += h * 2 + gap;
                ui.label(Rect::new(rect.x, y, rect.w, h), clock.ntp_permission.label(), ui.theme.text_dim, Align::Left); y += h + gap;
                if clock.can_ntp && clock.ntp_permission.editable() {
                    if ui.button(Rect::new(rect.x, y, ui.px(250), h), if clock.ntp { "Disable automatic time" } else { "Enable automatic time" }) { command = Some(DateTimeCommand::SetNtp(!clock.ntp)); }
                } else { ui.label(Rect::new(rect.x, y, rect.w, h), "Automatic time unavailable (no supported NTP service or permission)", ui.theme.text_dim, Align::Left); }
                y += h + gap;
                ui.label(Rect::new(rect.x, y, rect.w, h), if clock.ntp { "Manual clock editing is disabled while automatic time is enabled" } else { clock.time_permission.label() }, ui.theme.text_dim, Align::Left); y += h + gap;
                if !clock.ntp && clock.time_permission.editable() {
                    ui.text_field(&mut self.time, Rect::new(rect.x, y, ui.px(300), h), "Local YYYY-MM-DD HH:MM:SS", false, false); self.time_drawn = true;
                    if ui.button(Rect::new(rect.x + ui.px(310), y, ui.px(145), h), "Apply date/time") { command = Some(DateTimeCommand::SetTime(self.time.text().to_owned())); }
                    y += h + gap;
                }
                ui.label(Rect::new(rect.x, y, rect.w, h), "Global timezone (IANA region/city)", ui.theme.text, Align::Left); y += h + gap;
                ui.label(Rect::new(rect.x, y, rect.w, h), clock.zone_permission.label(), ui.theme.text_dim, Align::Left); y += h + gap;
                if clock.zone_permission.editable() && !self.state.zones.is_empty() {
                    ui.text_field(&mut self.search, Rect::new(rect.x, y, rect.w, h), "Filter timezone regions/cities", false, false); self.search_drawn = true; y += h + gap;
                    if self.filter.as_deref() != Some(self.search.text()) {
                        let query = self.search.text().to_lowercase();
                        self.filtered_zones.clear();
                        self.filtered_zones.extend(self.state.zones.iter().enumerate().filter_map(|(index, zone)| zone.to_lowercase().contains(&query).then_some(index)));
                        self.zone_heights.resize(self.filtered_zones.len(), h);
                        self.zones_scroll = ScrollState::new();
                        self.filter = Some(self.search.text().to_owned());
                    }
                    self.zone_heights.fill(h);
                    let list = Rect::new(rect.x, y, rect.w, h * 5);
                    ui.scroll_list(&mut self.zones_scroll, list, &self.zone_heights, |ui, index, row| {
                        let zone = &self.state.zones[self.filtered_zones[index]]; let panel = ui.theme.panel;
                        if self.selected_zone.as_ref() == Some(zone) { ui.theme.panel = ui.theme.selection; }
                        if ui.button(row, zone) {
                            self.selected_zone = Some(zone.clone());
                            self.zone_apply_label = format!("Apply timezone: {zone}");
                        }
                        ui.theme.panel = panel;
                    }); y += h * 5 + gap;
                    if let Some(zone) = &self.selected_zone {
                        if ui.button(Rect::new(rect.x, y, rect.w, h), &self.zone_apply_label) { command = Some(DateTimeCommand::SetTimezone(zone.clone())); }
                    } y += h + gap;
                }
            }
            ui.label(Rect::new(rect.x, y, rect.w, h), "Global date/time format (LC_TIME locale)", ui.theme.text, Align::Left); y += h + gap;
            ui.label(Rect::new(rect.x, y, rect.w, h), &self.locale_label, ui.theme.text_dim, Align::Left); y += h + gap;
            if let Some(permission) = &self.state.locale_permission {
                ui.label(Rect::new(rect.x, y, rect.w, h), permission.label(), ui.theme.text_dim, Align::Left); y += h + gap;
                if permission.editable() && !self.state.locales.is_empty() {
                    self.locale_heights.fill(h);
                    ui.scroll_list(&mut self.locales_scroll, Rect::new(rect.x, y, rect.w, h * 4), &self.locale_heights, |ui, index, row| {
                        let locale = &self.state.locales[index]; let panel = ui.theme.panel;
                        if self.selected_locale.as_ref() == Some(locale) { ui.theme.panel = ui.theme.selection; }
                        if ui.button(row, locale) {
                            self.selected_locale = Some(locale.clone());
                            self.locale_apply_label = format!("Apply date locale: {locale}");
                        }
                        ui.theme.panel = panel;
                    }); y += h * 4 + gap;
                    if let Some(locale) = &self.selected_locale {
                        if ui.button(Rect::new(rect.x, y, rect.w, h), &self.locale_apply_label) { command = Some(DateTimeCommand::SetLocale(locale.clone())); }
                    }
                }
            }
        });
        command
    }
}
pub async fn print() -> String {
    let state = DateTimeController::default()
        .run(DateTimeCommand::Refresh)
        .await;
    let mut lines = state.errors;
    if let Some(clock) = state.clock {
        lines.push(format!(
            "{} · {} · automatic={} · synchronized={}",
            clock.local, clock.timezone, clock.ntp, clock.synchronized
        ));
    }
    lines.push(format!(
        "Date locale: {} · {} system timezones",
        state.locale.as_deref().unwrap_or("unavailable"),
        state.zones.len()
    ));
    lines.join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_clock_rejects_dst_gap_and_overlap() {
        let database = TimeZoneDatabase::from_dir("/usr/share/zoneinfo").unwrap();
        assert!(local_microseconds("2026-03-29 02:30:00", "Europe/Berlin", &database).is_err());
        assert!(local_microseconds("2026-10-25 02:30:00", "Europe/Berlin", &database).is_err());
        let before = local_microseconds("2026-03-29 01:30:00", "Europe/Berlin", &database).unwrap();
        let after = local_microseconds("2026-03-29 03:30:00", "Europe/Berlin", &database).unwrap();
        assert_eq!(after - before, 3_600_000_000);
    }
    #[test]
    fn locale_date_change_preserves_unrelated_categories() {
        let original = vec![
            "LANG=en_US.UTF-8".into(),
            "LC_TIME=de_DE.UTF-8".into(),
            "LC_NUMERIC=C".into(),
        ];
        assert_eq!(
            locale_update(&original, "fr_FR.UTF-8"),
            vec!["LANG=en_US.UTF-8", "LC_NUMERIC=C", "LC_TIME=fr_FR.UTF-8"]
        );
        assert_eq!(time_locale(&original).as_deref(), Some("de_DE.UTF-8"));
        assert_eq!(
            time_locale(&["LANG=en_US.UTF-8".into()]).as_deref(),
            Some("en_US.UTF-8")
        );
    }
    #[test]
    fn global_override_has_precedence_over_date_category() {
        let values = vec![
            "LANG=en_US.UTF-8".into(),
            "LC_TIME=de_DE.UTF-8".into(),
            "LC_ALL=C".into(),
        ];
        assert_eq!(time_locale(&values).as_deref(), Some("C"));
    }
}
