//! Fetching Home's D-Bus data: compact drawer text, structured directory snapshots and brokered
//! file opening, shared between `--print` and the graphical Home Surface.
//!
//! `Drawer::WindowShelf` is deliberately not handled here: its data comes from the live
//! `lege_shell_v1` connection (`crate::wayland_bridge`), not a D-Bus session, and the callers
//! already have their own toplevel snapshot.

use lege_client::proxies::{
    AppIdentityProxy, AssociationsProxy, AudioProxy, BacklightProxy, BatteryProxy,
    FilesBrowseProxy, FilesOperationsProxy, FilesRecentsProxy, FilesSearchProxy, FilesTrashProxy,
    FilesVolumesProxy, HomeFieldProxy, LauncherProxy, LifecycleProxy, NotificationsProxy,
    ProjectsProxy,
};
use lege_field::{FieldDocument, FieldOp};

use crate::drawers;
use crate::drawers::standard_paths::StandardDrawer;
use crate::file_browser::{BrowserSources, DirectoryListing};
use crate::files_surface::{FilesLocation, PinnedKind, PinnedLocation};
use crate::home_surface::Drawer;

/// The one session-bus connection this process uses.
///
/// `zbus::Connection::session()` builds a *new* connection every time it is called — socket,
/// SASL handshake, `Hello`, and a background reader task — and the shell calls it from three
/// dozen places. A Home refresh alone fetches seven drawers, four widgets, the field, the
/// projects and the browser sources, so it was paying that handshake a dozen times over: measured
/// at roughly a millisecond each on this machine, against about one millisecond for the method
/// call it was made for. Half the cost of reading Home was opening connections to read it with.
///
/// Held behind an async lock rather than a `OnceCell` so that the first two callers to arrive
/// together share one connection instead of racing to build two, and so a connection that has
/// since closed can be replaced in place.
///
/// One entry per runtime. A `zbus::Connection` owns a reader task on the runtime that built it,
/// and a connection outliving that runtime does not report itself closed — it simply stops being
/// answered, and every call on it hangs. The shell has exactly one runtime, so this map holds
/// exactly one entry for the life of the process; the test binary gives each `#[tokio::test]` its
/// own runtime, and a single shared slot would have them evicting each other's live connections
/// on every call.
static SESSIONS: tokio::sync::Mutex<
    Option<std::collections::HashMap<tokio::runtime::Id, zbus::Connection>>,
> = tokio::sync::Mutex::const_new(None);

/// Connect to the session bus, or explain why not in one line.
///
/// Reuses this runtime's connection when there is a live one. A closed connection — the bus went
/// away, or the socket errored — is dropped and rebuilt rather than handed back, so a session
/// that comes back does not leave Home permanently unable to reach it.
pub async fn session() -> Result<zbus::Connection, String> {
    let runtime = tokio::runtime::Handle::current().id();
    let mut held = SESSIONS.lock().await;
    let connections = held.get_or_insert_with(std::collections::HashMap::new);
    // Closed connections go, whichever runtime they belonged to: this is the only place the map
    // is touched, so it is where an entry for a runtime that has gone away stops being kept.
    connections.retain(|_, connection| !connection.is_closed());
    if let Some(connection) = connections.get(&runtime) {
        return Ok(connection.clone());
    }
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| format!("service unavailable: no session bus ({error})"))?;
    connections.insert(runtime, connection.clone());
    Ok(connection)
}

/// Whether a LegeOS session daemon is answering on this machine's session bus.
///
/// Only tests ask this, and they ask because they describe what a drawer prints when *nothing*
/// answers — which can only be observed when nothing does. A developer running the session,
/// installed or nested via `.agent/scratch/nested-session.sh`, has a live `org.legeos.Session`, and
/// those drawers then return real content. That is the service working, not a regression, and
/// failing the build over it sends the next person hunting a bug that is not there.
pub async fn a_session_daemon_is_answering() -> bool {
    let Ok(connection) = session().await else {
        return false;
    };
    let Ok(proxy) = zbus::fdo::DBusProxy::new(&connection).await else {
        return false;
    };
    proxy
        .name_has_owner("org.legeos.Session".try_into().expect("a valid bus name"))
        .await
        .unwrap_or(false)
}

/// `service unavailable: <why>` — the single line a drawer prints instead of its contents.
pub fn unavailable(what: &str, error: &anyhow::Error) -> String {
    format!("{what} — service unavailable: {error:#}")
}

/// Render one D-Bus-backed drawer's text, exactly as `lege-shell --print <drawer>` does.
///
/// `drawer` must not be [`Drawer::WindowShelf`] — see the module doc for why that one is the
/// caller's own responsibility. [`Drawer::Network`] needs no bus at all and is answered directly.
pub async fn render_drawer(drawer: Drawer, limit: u32) -> String {
    fetch_drawer(drawer, limit).await.to_text()
}

/// One drawer at both densities — the headline the tile shows at every size, and the listing it
/// shows once it is tall enough. [`render_drawer`] is this flattened to text.
pub async fn fetch_drawer(drawer: Drawer, limit: u32) -> drawers::DrawerFeed {
    use drawers::DrawerFeed;
    if drawer == Drawer::Network {
        return drawers::network::feed();
    }

    let connection = match session().await {
        Ok(connection) => connection,
        Err(message) => {
            return DrawerFeed::unreachable(drawer.name(), message);
        }
    };
    let failed = |what: &str, error: &anyhow::Error| {
        DrawerFeed::unreachable(what, format!("service unavailable: {error:#}"))
    };

    match drawer {
        Drawer::Recent => match FilesRecentsProxy::new(&connection).await {
            Ok(proxy) => drawers::recent::feed(&proxy, limit)
                .await
                .unwrap_or_else(|error| failed("Recent", &error)),
            Err(error) => failed("Recent", &error.into()),
        },
        Drawer::Drives => match FilesVolumesProxy::new(&connection).await {
            Ok(proxy) => drawers::volumes::feed(&proxy)
                .await
                .unwrap_or_else(|error| failed("Drives", &error)),
            Err(error) => failed("Drives", &error.into()),
        },
        Drawer::Operations => match FilesOperationsProxy::new(&connection).await {
            Ok(proxy) => drawers::operations::feed(&proxy, limit)
                .await
                .unwrap_or_else(|error| failed("Operations", &error)),
            Err(error) => failed("Operations", &error.into()),
        },
        Drawer::Trash => match FilesTrashProxy::new(&connection).await {
            Ok(proxy) => drawers::trash::feed(&proxy, limit)
                .await
                .unwrap_or_else(|error| failed("Trash", &error)),
            Err(error) => failed("Trash", &error.into()),
        },
        Drawer::Projects => match ProjectsProxy::new(&connection).await {
            Ok(proxy) => drawers::projects::feed(&proxy)
                .await
                .unwrap_or_else(|error| failed("Projects", &error)),
            Err(error) => failed("Projects", &error.into()),
        },
        Drawer::Downloads | Drawer::Documents | Drawer::Media => {
            let standard = match drawer {
                Drawer::Downloads => StandardDrawer::Downloads,
                Drawer::Documents => StandardDrawer::Documents,
                _ => StandardDrawer::Media,
            };
            feed_standard(&connection, standard).await
        }
        // Handled above, before the connection was made.
        Drawer::Network => unreachable!(),
        Drawer::WindowShelf => unreachable!("see this module's doc: not this module's job"),
    }
}

async fn feed_standard(
    connection: &zbus::Connection,
    standard: StandardDrawer,
) -> drawers::DrawerFeed {
    use drawers::DrawerFeed;
    let Some(home) = drawers::standard_paths::home_dir() else {
        return DrawerFeed::unavailable(format!(
            "{} — service unavailable: $HOME is not set",
            standard.title()
        ));
    };
    let user_dirs = drawers::standard_paths::read_user_dirs(&home);
    let search = match FilesSearchProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => return DrawerFeed::unavailable(unavailable(standard.title(), &error.into())),
    };
    let mut feed =
        match drawers::standard_paths::feed(&search, standard, &home, user_dirs.as_deref()).await {
            Ok(feed) => feed,
            Err(error) => return DrawerFeed::unavailable(unavailable(standard.title(), &error)),
        };
    let browse = match FilesBrowseProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => return DrawerFeed::unavailable(unavailable(standard.title(), &error.into())),
    };
    // The headline is what the drawer is about: how much is in it. Counted here because this is
    // where the directories are actually listed.
    let mut entries = 0usize;
    let parsed_user_dirs = user_dirs
        .as_deref()
        .map(|text| drawers::standard_paths::parse_user_dirs(text, &home))
        .unwrap_or_default();
    for directory in drawers::standard_paths::directories_for(standard, &parsed_user_dirs, &home) {
        let json = match browse
            .list_directory(&directory.to_string_lossy(), false, 12)
            .await
        {
            Ok(json) => json,
            Err(error) => {
                feed.lines
                    .push(format!("  could not read {}: {error}", directory.display()));
                continue;
            }
        };
        let listing: DirectoryListing = match serde_json::from_str(&json) {
            Ok(listing) => listing,
            Err(error) => {
                feed.lines.push(format!(
                    "  malformed directory data for {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        if listing.entries.is_empty() {
            feed.lines.push("  (empty)".to_owned());
        }
        entries += listing.entries.len();
        for entry in listing.entries {
            feed.lines.push(format!(
                "  {:<7} {:<12} {}",
                if entry.is_dir() { "folder" } else { "file" },
                if entry.is_dir() {
                    "—".to_owned()
                } else {
                    crate::format::bytes(entry.size_bytes)
                },
                entry.name
            ));
        }
        if listing.truncated {
            feed.lines
                .push(format!("  … more than {} entries", listing.limit));
            // A truncated listing is a floor, not a total, and the headline says so.
            feed.headline = format!("{entries}+ items");
        }
    }
    if !feed.headline.ends_with("+ items") {
        feed.headline = drawers::DrawerFeed::count(entries, "item", "items", "empty");
    }
    feed
}

/// Every D-Bus-backed drawer's text in one call, for the Home Surface's background refresh.
/// `Drawer::WindowShelf` is absent — see the module doc.
/// Every installed application with what a launcher needs to offer it
/// (`AppIdentity1.ListApplications`).
///
/// `None` — not an empty list — when the session bus or sessiond is absent, because the two mean
/// opposite things to the person looking at the launcher: "nothing is installed" versus "I cannot
/// see what is installed". The widget says which.
pub async fn fetch_apps() -> Option<Vec<crate::app_search::AppEntry>> {
    let connection = session().await.ok()?;
    let proxy = AppIdentityProxy::new(&connection).await.ok()?;
    let rows = proxy.list_applications().await.ok()?;
    let mut variants: Vec<crate::app_search::AppEntry> = rows
        .into_iter()
        .filter_map(|row| {
            let (id, name, desktop_file_id, icon, categories, keywords, no_display) = row;
            Some(crate::app_search::AppEntry {
                id: id.parse().ok()?,
                name,
                desktop_file_id,
                icon,
                categories,
                keywords,
                no_display,
                logical: String::new(),
                variants: 1,
            })
        })
        .collect();
    // One row per application a person would name, rather than one per copy installed. A Firefox
    // from the distribution and a Firefox from Flathub are two variants of one thing, and a
    // search that offers both is a search asking a question nobody wanted asked. Everything a
    // *rank* is computed from — keywords, categories, the desktop id — still comes from the
    // default variant's own entry, because that is where the registry keeps it.
    let Ok(logical) = proxy.list_logical_apps().await else {
        // A daemon that does not group applications yet: the flat list is the honest answer.
        return Some(variants);
    };
    let mut grouped: Vec<crate::app_search::AppEntry> = Vec::new();
    let mut absorbed: Vec<String> = Vec::new();
    for (logical_id, name, icon, count, default_variant) in logical {
        let Some(entry) = variants
            .iter()
            .find(|entry| entry.id.as_str() == default_variant)
        else {
            continue;
        };
        let mut row = entry.clone();
        if !name.is_empty() {
            row.name = name;
        }
        if !icon.is_empty() {
            row.icon = icon;
        }
        row.variants = count.max(1);
        absorbed.push(default_variant);
        // Only a logical app with more than one copy costs a round trip, and only to learn which
        // other rows it speaks for. One copy is the overwhelmingly common case and asks nothing.
        if row.variants > 1
            && let Ok(rows) = proxy.list_variants(&logical_id).await
        {
            absorbed.extend(rows.into_iter().map(|(variant, _, _)| variant));
        }
        row.logical = logical_id;
        grouped.push(row);
    }
    if grouped.is_empty() {
        return Some(variants);
    }
    // Anything the grouping did not speak for stays reachable under its own name: a variant that
    // belongs to no logical app is still an application somebody installed.
    variants.retain(|entry| !absorbed.iter().any(|id| id == entry.id.as_str()));
    grouped.extend(variants);
    Some(grouped)
}

/// The copies of one logical application, default first, as `AppIdentity1.ListVariants` reports
/// them: the variant id, its display name, and where it came from.
///
/// Provenance is read for one key — `origin` — because that is the whole of what a person needs
/// to tell two copies of the same application apart in a menu. The rest of the object is there
/// for surfaces that have room for it.
pub async fn fetch_variants(logical: &str) -> Vec<(String, String, String)> {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Provenance {
        origin: String,
        version: String,
    }
    let Ok(connection) = session().await else {
        return Vec::new();
    };
    let Ok(proxy) = AppIdentityProxy::new(&connection).await else {
        return Vec::new();
    };
    proxy
        .list_variants(logical)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(variant, name, provenance_json)| {
            let provenance: Provenance = serde_json::from_str(&provenance_json).unwrap_or_default();
            let origin = match (provenance.origin.as_str(), provenance.version.as_str()) {
                ("", "") => String::new(),
                ("", version) => version.to_owned(),
                (origin, "") => origin.to_owned(),
                (origin, version) => format!("{origin} {version}"),
            };
            (variant, name, origin)
        })
        .collect()
}

/// The Session widget's content: whether this machine will actually log out, restart, shut down
/// or suspend, asked of `Lifecycle1` (which asks logind).
///
/// A widget that cannot reach the service says so rather than offering controls that fail on
/// click — see `crate::surface::widgets`.
pub async fn fetch_session_widget() -> crate::surface::widgets::WidgetSnapshot {
    use crate::surface::widgets::WidgetSnapshot;
    let Ok(connection) = session().await else {
        return WidgetSnapshot::unavailable("Session — no session bus");
    };
    let Ok(proxy) = LifecycleProxy::new(&connection).await else {
        return WidgetSnapshot::unavailable("Session — Lifecycle1 unavailable");
    };
    // Concurrently: each of these is a session-bus call that sessiond turns into a system-bus
    // call to logind, and asked in turn they cost three of those round trips rather than one.
    // An array literal awaits its elements in order, which is what made them serial.
    let (power_off, reboot, suspend) = tokio::join!(
        proxy.can_power_off(),
        proxy.can_reboot(),
        proxy.can_suspend()
    );
    let mut refused = Vec::new();
    let mut reachable = false;
    for (label, action, verdict) in [
        ("Shut down", "session.poweroff", power_off),
        ("Restart", "session.reboot", reboot),
        ("Suspend", "session.suspend", suspend),
    ] {
        match verdict {
            Ok(verdict) => {
                reachable = true;
                // Only a settled no. "challenge" means logind would authenticate the user, which
                // is a working control with a step in front of it, not a refused one. The
                // verdicts dim those controls; they are not the tile's body.
                if matches!(verdict.as_str(), "no" | "na") {
                    refused.push(action.to_owned());
                }
            }
            Err(error) => tracing::debug!(%error, %label, "Lifecycle1 query failed"),
        }
    }
    if !reachable {
        return WidgetSnapshot::unavailable("Session — logind unreachable");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "signed in".to_owned());
    WidgetSnapshot {
        headline: user,
        detail: vec!["Lock and leave from here".to_owned()],
        available: true,
        refused,
    }
}

/// Active notifications as a stable Home widget snapshot plus the structured rows the widget's
/// per-item controls need. The service owns ordering, expiry and dismissal; Home renders.
pub async fn fetch_notifications_widget() -> (
    crate::surface::widgets::WidgetSnapshot,
    Vec<crate::surface::widgets::notifications::NotificationRow>,
) {
    use crate::surface::widgets::WidgetSnapshot;
    use crate::surface::widgets::notifications::NotificationRow;
    let Ok(connection) = session().await else {
        return (
            WidgetSnapshot::unavailable("Notifications — no session bus"),
            Vec::new(),
        );
    };
    let Ok(proxy) = NotificationsProxy::new(&connection).await else {
        return (
            WidgetSnapshot::unavailable("Notifications — service unavailable"),
            Vec::new(),
        );
    };
    let Ok(rows) = proxy.list_active().await else {
        return (
            WidgetSnapshot::unavailable("Notifications — service unavailable"),
            Vec::new(),
        );
    };
    let structured: Vec<NotificationRow> = rows
        .iter()
        .filter_map(|row| serde_json::from_str::<serde_json::Value>(row).ok())
        .filter_map(|row| {
            let id = u32::try_from(row["id"].as_u64()?).ok()?;
            let actions = row["actions"]
                .as_array()
                .map(|actions| {
                    actions
                        .iter()
                        .filter_map(|pair| {
                            let key = pair.get(0)?.as_str()?.to_owned();
                            let label = pair.get(1)?.as_str()?.to_owned();
                            Some((key, label))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(NotificationRow {
                id,
                app: row["app_name"].as_str().unwrap_or("Application").to_owned(),
                summary: row["summary"].as_str().unwrap_or("Notification").to_owned(),
                actions,
            })
        })
        .collect();
    let detail = structured
        .iter()
        .map(|row| format!("{} — {}", row.app, row.summary))
        .collect();
    let snapshot = WidgetSnapshot {
        headline: match rows.len() {
            0 => "All caught up".to_owned(),
            1 => "1 notification".to_owned(),
            count => format!("{count} notifications"),
        },
        detail,
        available: true,
        refused: Vec::new(),
    };
    (snapshot, structured)
}

/// What the shell calls itself to `Files.Operations1`, so the journal records who asked.
pub const SHELL_APP_ID: &str = "org.legeos.Shell";

/// Poll one journalled operation until it stops, and hand back the record.
///
/// Polling rather than the `Finished` signal because this is a short-lived task that started the
/// operation and wants only its own: subscribing would mean filtering a stream of everybody's.
/// The interval is the service's own progress cadence; a copy of any size reports several times.
pub async fn follow_operation(
    proxy: &lege_client::proxies::FilesOperationsProxy<'_>,
    op_id: &str,
) -> anyhow::Result<lege_client::files::OperationRecord> {
    loop {
        let json = proxy.get(op_id).await?;
        let record: lege_client::files::OperationRecord = serde_json::from_str(&json)?;
        if record.state.terminal() {
            return Ok(record);
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// One line about a finished operation, in the person's terms.
///
/// A partial failure is never reported as success. The service records what it skipped and why it
/// stopped; both appear here, because "Moved 40 items" over an operation that actually moved 37
/// and gave up is the single most damaging thing a file manager can say — the person closes the
/// window believing the other three arrived.
pub fn describe_operation(summary: &str, record: &lege_client::files::OperationRecord) -> String {
    use lege_client::files::OperationState;
    let skipped = if record.skipped.is_empty() {
        String::new()
    } else {
        format!(
            " · {} skipped ({})",
            record.skipped.len(),
            record
                .skipped
                .iter()
                .take(3)
                .map(|path| path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    match record.state {
        // Committed with skips is not a plain success and does not read as one.
        OperationState::Committed => format!("{summary} — done{skipped}"),
        OperationState::Failed => format!(
            "{summary} — failed after {} of {} item(s): {}{skipped}",
            record.done_items,
            record.total_items,
            record.error.as_deref().unwrap_or("no reason given")
        ),
        OperationState::Cancelled => format!(
            "{summary} — cancelled after {} of {} item(s){skipped}",
            record.done_items, record.total_items
        ),
        OperationState::Undone => format!("{summary} — undone"),
        // Not terminal; `follow_operation` does not return these, and saying "done" would be a
        // guess about work still in flight.
        other => format!("{summary} — {other:?}{skipped}").to_lowercase(),
    }
}

/// The controls each service owns, refused together when that service has nothing to control.
///
/// `refused` is how a widget says "this button would not work" without removing it — the region
/// draws inert instead of vanishing, so a person is not left wondering where the control went.
const AUDIO_CONTROLS: [&str; 3] = ["volume.decrease", "volume.increase", "volume.mute"];
const BACKLIGHT_CONTROLS: [&str; 2] = ["brightness.decrease", "brightness.increase"];

/// The Settings widget's content: the battery, volume and brightness readings on one snapshot,
/// with the controls of any absent service refused.
///
/// One fetch over one connection, because it is one tile. Three separate widgets meant three
/// connections and three chances for the shelf to disagree with itself about whether the session
/// bus was there.
/// The session's screen-off delay in seconds, from `org.legeos.Power1`'s stored profile for
/// whichever power source the machine is on. Zero means never — and so does an unreachable
/// service, because a shell that cannot read the policy must not invent one.
///
/// The two fields read here mirror `lege_session_core::power::PowerProfile` (`screen_off_after`,
/// a `serde`-encoded `Option<Duration>`) and `lege_client::status::BatteryState::on_mains`.
/// They are read as JSON rather than through `lege-session-core` deliberately: that crate carries
/// `rusqlite`, and a shell has no business linking SQLite to learn one number.
pub async fn fetch_idle_timeout() -> u32 {
    let Ok(connection) = session().await else {
        return 0;
    };
    let on_mains = async {
        let proxy = BatteryProxy::new(&connection).await.ok()?;
        let json = proxy.get().await.ok()?;
        serde_json::from_str::<lege_client::status::BatteryState>(&json)
            .ok()
            .map(|state| state.on_mains)
    }
    .await
    // A machine with no battery service is a machine on mains.
    .unwrap_or(true);
    let source = if on_mains { "mains" } else { "battery" };

    let profile = async {
        let proxy = lege_client::proxies::PowerProxy::new(&connection)
            .await
            .ok()?;
        let json = proxy.get_profile(source).await.ok()?;
        serde_json::from_str::<serde_json::Value>(&json).ok()
    }
    .await;

    profile
        .as_ref()
        .and_then(|profile| profile.get("screen_off_after"))
        .and_then(|value| value.get("secs"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|secs| u32::try_from(secs).ok())
        .unwrap_or(0)
}

pub async fn fetch_settings_widget() -> crate::surface::widgets::WidgetSnapshot {
    use crate::surface::widgets::WidgetSnapshot;
    use crate::surface::widgets::settings;
    let Ok(connection) = session().await else {
        return WidgetSnapshot::unavailable("Settings — no session bus");
    };

    let battery = async {
        let proxy = BatteryProxy::new(&connection).await.ok()?;
        let json = proxy.get().await.ok()?;
        serde_json::from_str::<lege_client::status::BatteryState>(&json).ok()
    };
    let audio = async {
        let proxy = AudioProxy::new(&connection).await.ok()?;
        let json = proxy.get().await.ok()?;
        serde_json::from_str::<lege_client::status::AudioState>(&json).ok()
    };
    let backlight = async {
        let proxy = BacklightProxy::new(&connection).await.ok()?;
        let json = proxy.get().await.ok()?;
        serde_json::from_str::<lege_client::status::BacklightState>(&json).ok()
    };
    let (battery, audio, backlight) = tokio::join!(battery, audio, backlight);

    // Nothing answering at all is a different thing from a machine without a battery, and the
    // tile has to say which. Only the first is unavailable.
    if battery.is_none() && audio.is_none() && backlight.is_none() {
        return WidgetSnapshot::unavailable("Settings — services unavailable");
    }

    let mut refused = Vec::new();
    if audio.as_ref().is_none_or(|state| !state.present) {
        refused.extend(AUDIO_CONTROLS.iter().map(|id| (*id).to_owned()));
    }
    if backlight.as_ref().is_none_or(|state| !state.present) {
        refused.extend(BACKLIGHT_CONTROLS.iter().map(|id| (*id).to_owned()));
    }
    // "Unavailable" for the one service that did not answer while the others did — a blank line
    // would read as a reading of nothing, which is the one thing it is not.
    let unavailable = || "Unavailable".to_owned();
    settings::snapshot(
        [
            battery
                .as_ref()
                .map_or_else(unavailable, lege_client::status::BatteryState::reading),
            audio
                .as_ref()
                .map_or_else(unavailable, lege_client::status::AudioState::headline),
            backlight
                .as_ref()
                .map_or_else(unavailable, lege_client::status::BacklightState::headline),
        ],
        refused,
    )
}

/// `Projects1.List`, or none when sessiond is absent.
/// The subset of `interfaces/schemas/project.schema.json` the shell reads. Permissive by
/// construction: every field defaults, so a daemon that predates or postdates this still hands
/// back something usable.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ProjectPayload {
    name: String,
    root: String,
    state: String,
}

pub async fn fetch_projects() -> Vec<crate::field_model::ProjectInfo> {
    let Ok(connection) = session().await else {
        return Vec::new();
    };
    let Ok(proxy) = ProjectsProxy::new(&connection).await else {
        return Vec::new();
    };
    // One round trip for every project, rather than a list plus a Describe apiece. The payload
    // is the schema in `interfaces/schemas/project.schema.json`, and every field of it is
    // optional on read: a shell talking to a daemon that predates or postdates it degrades to a
    // name rather than to nothing.
    let current = proxy.get_current().await.unwrap_or_default();
    proxy
        .list_detailed()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, json)| {
            let payload: ProjectPayload = serde_json::from_str(&json).unwrap_or_default();
            crate::field_model::ProjectInfo {
                is_open: !id.is_empty() && id == current,
                archived: payload.state == "archived",
                name: payload.name,
                root: payload.root,
                id,
            }
        })
        .collect()
}

/// `HomeField1.Get`: the persisted field for `output`, created at `cols`×`rows` if new. `None`
/// when sessiond is absent — the shell then keeps an in-session document instead.
pub async fn fetch_field(output: &str, cols: u16, rows: u16) -> Option<FieldDocument> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    let json = proxy.get(output, cols, rows).await.ok()?;
    serde_json::from_str(&json).ok()
}

/// `HomeField1.Apply`: persist one op; the new revision, or `None` when refused or unreachable.
pub async fn apply_field_op(output: &str, op: &FieldOp) -> Option<u64> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    let json = serde_json::to_string(op).ok()?;
    match proxy.apply(output, &json).await {
        Ok(revision) => Some(revision),
        Err(error) => {
            tracing::warn!(%error, "HomeField1.Apply refused");
            None
        }
    }
}

/// `HomeField1.Reset`: discard the stored layout and return the fresh document that replaces it.
/// `None` when the service is unreachable — there is nothing sensible to do locally, because the
/// point of a reset is that the *stored* document goes away.
pub async fn reset_field(output: &str, cols: u16, rows: u16) -> Option<FieldDocument> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    match proxy.reset(output, cols, rows).await {
        Ok(json) => serde_json::from_str(&json).ok(),
        Err(error) => {
            tracing::warn!(%error, "HomeField1.Reset refused");
            None
        }
    }
}

/// `HomeField1.Undo`: the document after undoing the last op, or `None` if nothing to undo or
/// the service is unreachable.
pub async fn undo_field(output: &str) -> Option<FieldDocument> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    let json = proxy.undo(output).await.ok()?;
    if json.is_empty() {
        return None;
    }
    serde_json::from_str(&json).ok()
}

pub async fn fetch_all(limit: u32) -> Vec<(Drawer, drawers::DrawerFeed)> {
    let mut out = Vec::with_capacity(Drawer::DEFAULT.len() - 1);
    for drawer in Drawer::DEFAULT {
        if drawer == Drawer::WindowShelf {
            continue;
        }
        out.push((drawer, fetch_drawer(drawer, limit).await));
    }
    out
}

/// The pinned locations the Files object offers: the machine's own file feeds, then the same XDG
/// places and mounted volumes the standalone manager renders. `Files.Browse1` owns the source
/// list; Home only decides the order they are offered in.
pub async fn fetch_browser_sources() -> BrowserSources {
    let fallback = local_browser_sources();
    let Ok(connection) = session().await else {
        return fallback;
    };
    let Ok(proxy) = FilesBrowseProxy::new(&connection).await else {
        return fallback;
    };
    let Ok(json) = proxy.roots().await else {
        return fallback;
    };
    let Ok(shared) = serde_json::from_str::<lege_client::files::BrowserSources>(&json) else {
        return fallback;
    };
    let mut sources = BrowserSources {
        pinned: leading_feeds(),
    };
    for root in shared.roots {
        let detail = root.volume.as_ref().map_or_else(
            || {
                if root.available {
                    "Standard folder".to_owned()
                } else {
                    "Not present".to_owned()
                }
            },
            |volume| {
                format!(
                    "{} free of {} · {}{}",
                    crate::format::bytes(volume.free_bytes),
                    crate::format::bytes(volume.total_bytes),
                    volume.fs_type,
                    if volume.read_only {
                        " · read-only"
                    } else {
                        ""
                    }
                )
            },
        );
        sources.pinned.push(PinnedLocation {
            label: root.label,
            detail,
            location: FilesLocation::Directory(root.path),
            kind: match root.kind {
                lege_client::files::RootKind::Place => PinnedKind::Place,
                lege_client::files::RootKind::Drive => PinnedKind::Drive,
            },
        });
    }
    sources.pinned.extend(trailing_feeds());
    sources
}

/// The feeds that come before the folders: what was used lately is the answer to "where is that
/// thing I had open", which is the question people arrive with most often.
fn leading_feeds() -> Vec<PinnedLocation> {
    vec![PinnedLocation {
        label: "Recent".to_owned(),
        detail: "Lately opened and saved".to_owned(),
        location: FilesLocation::Recent,
        kind: PinnedKind::Feed,
    }]
}

/// And the ones that come after them: what is being moved, and what was thrown away.
fn trailing_feeds() -> Vec<PinnedLocation> {
    vec![
        PinnedLocation {
            label: "Operations".to_owned(),
            detail: "Copies and moves".to_owned(),
            location: FilesLocation::Operations,
            kind: PinnedKind::Feed,
        },
        PinnedLocation {
            label: "Trash".to_owned(),
            detail: "Restorable".to_owned(),
            location: FilesLocation::Trash,
            kind: PinnedKind::Feed,
        },
    ]
}

fn local_browser_sources() -> BrowserSources {
    let Some(home) = drawers::standard_paths::home_dir() else {
        return BrowserSources {
            pinned: leading_feeds()
                .into_iter()
                .chain(trailing_feeds())
                .collect(),
        };
    };
    let user_dirs = drawers::standard_paths::read_user_dirs(&home)
        .map(|text| drawers::standard_paths::parse_user_dirs(&text, &home))
        .unwrap_or_default();
    let downloads =
        drawers::standard_paths::directories_for(StandardDrawer::Downloads, &user_dirs, &home);
    let documents =
        drawers::standard_paths::directories_for(StandardDrawer::Documents, &user_dirs, &home);
    let media = drawers::standard_paths::directories_for(StandardDrawer::Media, &user_dirs, &home);
    let mut sources = BrowserSources {
        pinned: leading_feeds(),
    };
    sources.pinned.push(PinnedLocation {
        label: "Home".to_owned(),
        detail: "Personal files".to_owned(),
        location: FilesLocation::Directory(home),
        kind: PinnedKind::Place,
    });
    add_place(&mut sources, "Downloads", &downloads);
    add_place(&mut sources, "Documents", &documents);
    for (index, path) in media.iter().enumerate() {
        let label = ["Pictures", "Music", "Videos"]
            .get(index)
            .copied()
            .unwrap_or("Media");
        sources.pinned.push(PinnedLocation {
            label: label.to_owned(),
            detail: "Media folder".to_owned(),
            location: FilesLocation::Directory(path.clone()),
            kind: PinnedKind::Place,
        });
    }
    sources.pinned.extend(trailing_feeds());
    sources
}

fn add_place(sources: &mut BrowserSources, label: &str, paths: &[std::path::PathBuf]) {
    for path in paths {
        sources.pinned.push(PinnedLocation {
            label: label.to_owned(),
            detail: "Standard folder".to_owned(),
            location: FilesLocation::Directory(path.clone()),
            kind: PinnedKind::Place,
        });
    }
}

/// Ask `lege-filed` for one real directory snapshot.
/// One bounded thumbnail for the browse grid: `Files.Previews1.Render` draws it server-side
/// and this decodes the PNG here, in the async task — the render thread never sees a file.
/// `None` for unsupported files, an unreachable service, or an undecodable answer: a grid cell
/// simply keeps its kind chip.
pub async fn file_thumbnail(path: &std::path::Path) -> Option<pixelkit_raster::Bitmap> {
    let connection = session().await.ok()?;
    let proxy = lege_client::proxies::files::FilesPreviewsProxy::new(&connection)
        .await
        .ok()?;
    let (_, png) = proxy.render(&path.to_string_lossy(), 256).await.ok()?;
    if png.is_empty() {
        return None;
    }
    pixelkit_raster::png::decode(&png).ok()
}

pub async fn browse_directory(
    path: &std::path::Path,
    show_hidden: bool,
) -> Result<DirectoryListing, String> {
    let connection = session().await?;
    let proxy = FilesBrowseProxy::new(&connection)
        .await
        .map_err(|error| format!("file service unavailable: {error}"))?;
    let json = proxy
        .list_directory(&path.to_string_lossy(), show_hidden, 500)
        .await
        .map_err(|error| format!("could not list {}: {error}", path.display()))?;
    serde_json::from_str(&json)
        .map_err(|error| format!("malformed directory data for {}: {error}", path.display()))
}

/// `Providers1.Report`'s resolution, as far as a client needs it: who fills the role, if anyone.
///
/// Mirrors `lege_session_core::providers::Resolution`'s `#[serde(tag = "kind")]` wire shape.
/// The shell keeps its own copy of the shape for the same reason `lege-settings` does — the JSON
/// is the contract, and depending on the daemon crate for it would drag SQLite into a shell.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
enum ProviderResolution {
    Selected { app_id: String },
    DefaultedByPriority { app_id: String },
    SelectionStale {},
    NoProvider,
}

impl ProviderResolution {
    /// The application allowed to fill the role. A stale selection is nobody: the application it
    /// names is not installed, and launching it would fail in a way nothing here could explain.
    fn provider(self) -> Option<String> {
        match self {
            Self::Selected { app_id } | Self::DefaultedByPriority { app_id } => Some(app_id),
            Self::SelectionStale {} | Self::NoProvider => None,
        }
    }
}

/// Who fills one exclusive role (`org.legeos.Providers1`), or `None` when nobody does — which
/// includes the service not answering, because a shell with no session daemon has no provider
/// either way.
///
/// `Report` rather than a per-role call because that is the whole surface Providers1 offers for
/// reading a resolution, and one round trip answers for every role.
pub async fn selected_provider(role: &str) -> Option<String> {
    let connection = session().await.ok()?;
    let proxy = lege_client::proxies::ProvidersProxy::new(&connection)
        .await
        .ok()?;
    let report = proxy.report().await.ok()?;
    resolution_for(&report, role)
}

/// The provider for `role` in a `Providers1.Report` answer. Pure, so the parse is testable
/// without a bus.
fn resolution_for(report: &[(String, String)], role: &str) -> Option<String> {
    let (_, json) = report.iter().find(|(name, _)| name == role)?;
    serde_json::from_str::<ProviderResolution>(json)
        .ok()?
        .provider()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenFileResult {
    pub app_id: String,
    pub mime: String,
    pub instance: String,
}

#[async_trait::async_trait]
pub trait BrowserAssociations: Send + Sync {
    async fn resolve_file(&self, path: &str) -> Result<(String, String, String), String>;
}

#[async_trait::async_trait]
impl BrowserAssociations for AssociationsProxy<'_> {
    async fn resolve_file(&self, path: &str) -> Result<(String, String, String), String> {
        self.resolve_file(path, "")
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
pub trait BrowserLauncher: Send + Sync {
    async fn launch(&self, app_id: &str, arguments: Vec<String>) -> Result<String, String>;
}

#[async_trait::async_trait]
impl BrowserLauncher for LauncherProxy<'_> {
    async fn launch(&self, app_id: &str, arguments: Vec<String>) -> Result<String, String> {
        self.launch(app_id, arguments, "")
            .await
            .map_err(|error| error.to_string())
    }
}

/// Resolve first, launch second. An empty association is a visible result, never permission to
/// bypass the broker with `xdg-open` or a shell command.
pub async fn brokered_file_open(
    associations: &impl BrowserAssociations,
    launcher: &impl BrowserLauncher,
    path: &std::path::Path,
) -> Result<OpenFileResult, String> {
    let target = path.to_string_lossy().into_owned();
    let (app_id, mime, reason) = associations
        .resolve_file(&target)
        .await
        .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
    if app_id.is_empty() {
        return Err(format!(
            "No application is associated with {} ({}; {reason})",
            path.display(),
            if mime.is_empty() {
                "unknown type"
            } else {
                &mime
            }
        ));
    }
    let instance = launcher
        .launch(&app_id, vec![target])
        .await
        .map_err(|error| format!("could not open {} with {app_id}: {error}", path.display()))?;
    Ok(OpenFileResult {
        app_id,
        mime,
        instance,
    })
}

pub async fn open_file(path: &std::path::Path) -> Result<OpenFileResult, String> {
    let connection = session().await?;
    let associations = AssociationsProxy::new(&connection)
        .await
        .map_err(|error| format!("association service unavailable: {error}"))?;
    let launcher = LauncherProxy::new(&connection)
        .await
        .map_err(|error| format!("launcher unavailable: {error}"))?;
    let result = brokered_file_open(&associations, &launcher, path).await?;
    if let Ok(recents) = FilesRecentsProxy::new(&connection).await {
        let _ = recents
            .record(
                &path.to_string_lossy(),
                &result.app_id,
                "opened",
                &result.mime,
            )
            .await;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeAssociations((String, String, String));

    #[async_trait::async_trait]
    impl BrowserAssociations for FakeAssociations {
        async fn resolve_file(&self, _path: &str) -> Result<(String, String, String), String> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct FakeLauncher(Mutex<Vec<(String, Vec<String>)>>);

    #[async_trait::async_trait]
    impl BrowserLauncher for FakeLauncher {
        async fn launch(&self, app_id: &str, arguments: Vec<String>) -> Result<String, String> {
            self.0.lock().unwrap().push((app_id.to_owned(), arguments));
            Ok("instance-1".to_owned())
        }
    }

    /// The four resolutions a role can be in, as the daemon writes them, read back as "who may
    /// be launched". A stale selection is nobody: the application it names is not installed.
    #[test]
    fn the_providers_report_says_who_fills_a_role() {
        let report = vec![
            (
                "terminal".to_owned(),
                r#"{"kind":"selected","app_id":"desktop-import.kitty.desktop"}"#.to_owned(),
            ),
            (
                "file-search".to_owned(),
                r#"{"kind":"defaulted-by-priority","app_id":"desktop-import.fsearch.desktop","competitors":[]}"#
                    .to_owned(),
            ),
            (
                "greeter".to_owned(),
                r#"{"kind":"selection-stale","app_id":"gone.away"}"#.to_owned(),
            ),
            ("lock-screen".to_owned(), r#"{"kind":"no-provider"}"#.to_owned()),
        ];
        assert_eq!(
            resolution_for(&report, "terminal").as_deref(),
            Some("desktop-import.kitty.desktop")
        );
        assert_eq!(
            resolution_for(&report, "file-search").as_deref(),
            Some("desktop-import.fsearch.desktop")
        );
        assert_eq!(resolution_for(&report, "greeter"), None);
        assert_eq!(resolution_for(&report, "lock-screen"), None);
        assert_eq!(resolution_for(&report, "input-method"), None);
    }

    #[tokio::test]
    async fn the_bus_free_drawer_renders_without_any_session_bus() {
        let network = render_drawer(Drawer::Network, 10).await;
        assert!(network.contains("not wired to a service yet"));
    }

    #[tokio::test]
    async fn every_bus_backed_drawer_degrades_readably_with_no_bus() {
        if a_session_daemon_is_answering().await {
            eprintln!(
                "skipped: org.legeos.Session is answering on this bus, so the no-service path \
                 cannot be observed here"
            );
            return;
        }
        for drawer in Drawer::DEFAULT {
            if matches!(drawer, Drawer::Network | Drawer::WindowShelf) {
                continue;
            }
            let text = render_drawer(drawer, 5).await;
            assert!(
                text.contains("unavailable"),
                "{drawer:?} did not degrade readably: {text}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_all_covers_every_drawer_except_the_window_shelf() {
        let all = fetch_all(5).await;
        assert_eq!(all.len(), Drawer::DEFAULT.len() - 1);
        assert!(!all.iter().any(|(drawer, _)| *drawer == Drawer::WindowShelf));
    }

    #[tokio::test]
    async fn brokered_file_open_resolves_then_launches_the_canonical_app() {
        let associations = FakeAssociations((
            "org.example.Reader".into(),
            "application/pdf".into(),
            "global-mime-default".into(),
        ));
        let launcher = FakeLauncher::default();
        let result = brokered_file_open(
            &associations,
            &launcher,
            std::path::Path::new("/home/test/paper.pdf"),
        )
        .await
        .unwrap();
        assert_eq!(result.app_id, "org.example.Reader");
        assert_eq!(result.instance, "instance-1");
        assert_eq!(
            *launcher.0.lock().unwrap(),
            vec![(
                "org.example.Reader".into(),
                vec!["/home/test/paper.pdf".into()]
            )]
        );
    }

    #[tokio::test]
    async fn the_session_connection_is_built_once_and_handed_back() {
        let Ok(first) = session().await else {
            // No session bus here (a build container). Nothing to assert about reuse.
            return;
        };
        let second = session().await.expect("the cached connection");
        assert_eq!(
            first.unique_name(),
            second.unique_name(),
            "a second caller must get the connection the first one built, not a new socket, \
             a new handshake and a new reader task"
        );
    }

    #[tokio::test]
    async fn a_second_runtime_never_inherits_the_first_runtime_s_connection() {
        // Whatever this test binary's other tests left in the cache belongs to their runtimes; a
        // connection whose runtime is gone does not report itself closed, it just stops being
        // answered. The guard is the runtime id, and the observable consequence is that this
        // call returns something that actually works.
        let Ok(connection) = session().await else {
            return;
        };
        let proxy = zbus::fdo::DBusProxy::new(&connection)
            .await
            .expect("a proxy on a live connection");
        proxy
            .list_names()
            .await
            .expect("a call on this runtime's own connection is answered");
    }

    #[tokio::test]
    async fn brokered_file_open_never_bypasses_an_unresolved_association() {
        let associations = FakeAssociations((
            String::new(),
            "application/x-unknown".into(),
            "ask-user".into(),
        ));
        let launcher = FakeLauncher::default();
        let error = brokered_file_open(
            &associations,
            &launcher,
            std::path::Path::new("/home/test/unknown.bin"),
        )
        .await
        .unwrap_err();
        assert!(error.contains("No application is associated"));
        assert!(error.contains("ask-user"));
        assert!(launcher.0.lock().unwrap().is_empty());
    }
}
