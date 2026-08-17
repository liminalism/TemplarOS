//! D-Bus names, paths and interfaces — the single vocabulary shared by servers and clients.
//!
//! One bus name per daemon; domain-scoped, versioned interfaces at separate object paths, so a
//! future process split only relocates a bus name (ADR-0001).

/// User session policy daemon (`lege-sessiond`).
pub mod session {
    pub const BUS_NAME: &str = "org.legeos.Session";
    pub const ROOT_PATH: &str = "/org/legeos/Session";

    pub const APP_IDENTITY_PATH: &str = "/org/legeos/Session/AppIdentity";
    pub const APP_IDENTITY_IFACE: &str = "org.legeos.AppIdentity1";

    pub const LAUNCHER_PATH: &str = "/org/legeos/Session/Launcher";
    pub const LAUNCHER_IFACE: &str = "org.legeos.Launcher1";

    pub const ASSOCIATIONS_PATH: &str = "/org/legeos/Session/Associations";
    pub const ASSOCIATIONS_IFACE: &str = "org.legeos.Associations1";

    pub const PROVIDERS_PATH: &str = "/org/legeos/Session/Providers";
    pub const PROVIDERS_IFACE: &str = "org.legeos.Providers1";

    pub const HISTORY_PATH: &str = "/org/legeos/Session/History";
    pub const HISTORY_IFACE: &str = "org.legeos.History1";

    pub const SETTINGS_PATH: &str = "/org/legeos/Session/Settings";
    pub const SETTINGS_IFACE: &str = "org.legeos.Settings1";

    pub const PROJECTS_PATH: &str = "/org/legeos/Session/Projects";
    pub const PROJECTS_IFACE: &str = "org.legeos.Projects1";

    pub const POWER_PATH: &str = "/org/legeos/Session/Power";
    pub const POWER_IFACE: &str = "org.legeos.Power1";

    pub const LOCATION_PATH: &str = "/org/legeos/Session/Location";
    pub const LOCATION_IFACE: &str = "org.legeos.Location1";

    /// Also claimed by sessiond when it is the selected notification-server provider.
    pub const FDO_NOTIFICATIONS_BUS_NAME: &str = "org.freedesktop.Notifications";
}

/// Resource governor (`lege-resourced`).
pub mod resources {
    pub const BUS_NAME: &str = "org.legeos.Resources";
    pub const PATH: &str = "/org/legeos/Resources";
    pub const IFACE: &str = "org.legeos.Resources1";
}

/// File service (`lege-filed`).
pub mod files {
    pub const BUS_NAME: &str = "org.legeos.Files";
    pub const ROOT_PATH: &str = "/org/legeos/Files";
    pub const VOLUMES_IFACE: &str = "org.legeos.Files.Volumes1";
    pub const OPERATIONS_IFACE: &str = "org.legeos.Files.Operations1";
    pub const RECENTS_IFACE: &str = "org.legeos.Files.Recents1";
    pub const SEARCH_IFACE: &str = "org.legeos.Files.Search1";
}

/// Privileged host daemon (`lege-hostd`, system bus).
pub mod host {
    pub const BUS_NAME: &str = "org.legeos.Host";
    pub const ROOT_PATH: &str = "/org/legeos/Host";
    pub const UPDATES_IFACE: &str = "org.legeos.Host.Updates1";
    pub const DEPLOYMENTS_IFACE: &str = "org.legeos.Host.Deployments1";
    pub const DRIVERS_IFACE: &str = "org.legeos.Host.Drivers1";
    pub const STORAGE_IFACE: &str = "org.legeos.Host.Storage1";
    pub const HARDWARE_IFACE: &str = "org.legeos.Host.Hardware1";
    pub const POLKIT_ACTION_PREFIX: &str = "org.legeos.host";
}

/// Environment variables injected into every launched application instance.
pub mod env {
    pub const INSTANCE_ID: &str = "LEGEOS_INSTANCE_ID";
    pub const APP_ID: &str = "LEGEOS_APP_ID";
}
