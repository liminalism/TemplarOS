//! D-Bus names, paths and interfaces — the single vocabulary shared by servers and clients.
//!
//! One bus name per daemon; domain-scoped, versioned interfaces at separate object paths, so a
//! future process split only relocates a bus name (ADR-0001).

/// User session policy daemon (`templar-sessiond`).
pub mod session {
    pub const BUS_NAME: &str = "org.templaros.Session";
    pub const ROOT_PATH: &str = "/org/templaros/Session";

    pub const APP_IDENTITY_PATH: &str = "/org/templaros/Session/AppIdentity";
    pub const APP_IDENTITY_IFACE: &str = "org.templaros.AppIdentity1";

    pub const LAUNCHER_PATH: &str = "/org/templaros/Session/Launcher";
    pub const LAUNCHER_IFACE: &str = "org.templaros.Launcher1";

    pub const ASSOCIATIONS_PATH: &str = "/org/templaros/Session/Associations";
    pub const ASSOCIATIONS_IFACE: &str = "org.templaros.Associations1";

    pub const PROVIDERS_PATH: &str = "/org/templaros/Session/Providers";
    pub const PROVIDERS_IFACE: &str = "org.templaros.Providers1";

    pub const HISTORY_PATH: &str = "/org/templaros/Session/History";
    pub const HISTORY_IFACE: &str = "org.templaros.History1";

    pub const SETTINGS_PATH: &str = "/org/templaros/Session/Settings";
    pub const SETTINGS_IFACE: &str = "org.templaros.Settings1";

    pub const PROJECTS_PATH: &str = "/org/templaros/Session/Projects";
    pub const PROJECTS_IFACE: &str = "org.templaros.Projects1";

    pub const HOME_FIELD_PATH: &str = "/org/templaros/Session/HomeField";
    pub const HOME_FIELD_IFACE: &str = "org.templaros.HomeField1";

    pub const HOME_GARDEN_PATH: &str = "/org/templaros/Session/HomeGarden";
    pub const HOME_GARDEN_IFACE: &str = "org.templaros.HomeGarden1";

    pub const POWER_PATH: &str = "/org/templaros/Session/Power";
    pub const POWER_IFACE: &str = "org.templaros.Power1";

    pub const AUDIO_PATH: &str = "/org/templaros/Session/Audio";
    pub const AUDIO_IFACE: &str = "org.templaros.Audio1";

    pub const BACKLIGHT_PATH: &str = "/org/templaros/Session/Backlight";
    pub const BACKLIGHT_IFACE: &str = "org.templaros.Backlight1";

    pub const BATTERY_PATH: &str = "/org/templaros/Session/Battery";
    pub const BATTERY_IFACE: &str = "org.templaros.Battery1";

    pub const LIFECYCLE_PATH: &str = "/org/templaros/Session/Lifecycle";
    pub const LIFECYCLE_IFACE: &str = "org.templaros.Session.Lifecycle1";

    pub const LOCATION_PATH: &str = "/org/templaros/Session/Location";
    pub const LOCATION_IFACE: &str = "org.templaros.Location1";

    pub const NOTIFICATIONS_PATH: &str = "/org/templaros/Session/Notifications";
    pub const NOTIFICATIONS_IFACE: &str = "org.templaros.Notifications1";

    /// Also claimed by sessiond when it is the selected notification-server provider.
    pub const FDO_NOTIFICATIONS_BUS_NAME: &str = "org.freedesktop.Notifications";
}

/// Resource governor (`templar-resourced`).
pub mod resources {
    pub const BUS_NAME: &str = "org.templaros.Resources";
    pub const PATH: &str = "/org/templaros/Resources";
    pub const IFACE: &str = "org.templaros.Resources1";
}

/// File service (`templar-filed`).
pub mod files {
    pub const BUS_NAME: &str = "org.templaros.Files";
    pub const ROOT_PATH: &str = "/org/templaros/Files";
    pub const VOLUMES_IFACE: &str = "org.templaros.Files.Volumes1";
    pub const PREVIEWS_IFACE: &str = "org.templaros.Files.Previews1";
    pub const OPERATIONS_IFACE: &str = "org.templaros.Files.Operations1";
    pub const RECENTS_IFACE: &str = "org.templaros.Files.Recents1";
    pub const SEARCH_IFACE: &str = "org.templaros.Files.Search1";
    pub const TRASH_IFACE: &str = "org.templaros.Files.Trash1";
}

/// Privileged host daemon (`templar-hostd`, system bus).
pub mod host {
    pub const BUS_NAME: &str = "org.templaros.Host";
    pub const ROOT_PATH: &str = "/org/templaros/Host";
    pub const UPDATES_IFACE: &str = "org.templaros.Host.Updates1";
    pub const DEPLOYMENTS_IFACE: &str = "org.templaros.Host.Deployments1";
    pub const DRIVERS_IFACE: &str = "org.templaros.Host.Drivers1";
    pub const STORAGE_IFACE: &str = "org.templaros.Host.Storage1";
    pub const HARDWARE_IFACE: &str = "org.templaros.Host.Hardware1";
    pub const POLKIT_ACTION_PREFIX: &str = "org.templaros.host";
}

/// The checkpoint protocol, as it appears on the bus.
///
/// Every other interface here is served by a daemon and called by clients. This one is the other
/// way round: an *application instance* serves it, and `templar-sessiond` calls into it on the
/// governor's behalf (`docs/protocols/checkpoint.md`). That inversion is why it needs its own
/// addressing rule.
///
/// An instance claims [`instance_bus_name`] for itself at startup — a name derived from the
/// `InstanceId` it was launched with, so nothing has to be registered and no lookup table can go
/// stale. The session daemon needs no directory: it knows the instance, so it knows the name.
/// It should still verify before trusting an answer, by resolving that name's PID to a cgroup
/// the same way `AppIdentity1` resolves a window — a name is a claim, and claims are checked.
pub mod checkpoint {
    use crate::InstanceId;

    pub const IFACE: &str = "org.templaros.Checkpoint1";
    pub const PATH: &str = "/org/templaros/Checkpoint";

    /// Prefix of the per-instance bus name. The trailing `i` is not decoration: a D-Bus name
    /// element may not begin with a digit, and an `InstanceId`'s compact form is 32 hex
    /// characters that frequently does.
    pub const BUS_NAME_PREFIX: &str = "org.templaros.Instance.i";

    /// The bus name an instance claims for itself.
    pub fn instance_bus_name(instance: InstanceId) -> String {
        format!("{BUS_NAME_PREFIX}{}", instance.compact())
    }

    /// Recover the instance from a name produced by [`instance_bus_name`].
    pub fn instance_from_bus_name(name: &str) -> Option<InstanceId> {
        InstanceId::parse(name.strip_prefix(BUS_NAME_PREFIX)?)
    }
}

/// Environment variables injected into every launched application instance.
pub mod env {
    pub const INSTANCE_ID: &str = "TEMPLAROS_INSTANCE_ID";
    pub const APP_ID: &str = "TEMPLAROS_APP_ID";
}

#[cfg(test)]
mod tests {
    use super::checkpoint;
    use crate::InstanceId;

    #[test]
    fn an_instance_bus_name_round_trips() {
        let instance = InstanceId::new();
        let name = checkpoint::instance_bus_name(instance);
        assert_eq!(checkpoint::instance_from_bus_name(&name), Some(instance));
        assert_eq!(
            checkpoint::instance_from_bus_name("org.templaros.Session"),
            None
        );
    }

    #[test]
    fn an_instance_bus_name_is_a_valid_dbus_name() {
        // Each dot-separated element must be non-empty, may contain only [A-Za-z0-9_-], and must
        // not begin with a digit — which is exactly why the prefix ends in a letter.
        let name = checkpoint::instance_bus_name(InstanceId::new());
        assert!(name.len() <= 255);
        let elements: Vec<&str> = name.split('.').collect();
        assert!(elements.len() >= 2);
        for element in elements {
            assert!(!element.is_empty(), "{name}");
            assert!(
                !element.starts_with(|c: char| c.is_ascii_digit()),
                "element {element:?} of {name} begins with a digit"
            );
            assert!(
                element
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
                "element {element:?} of {name} has an invalid character"
            );
        }
    }
}
