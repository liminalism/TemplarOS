//! Proxies for `lege-sessiond` (`org.legeos.Session`): identity, launching, associations,
//! providers, history, settings, projects, power, location and background permissions.
//!
//! Kept in sync with `interfaces/dbus/org.legeos.*.xml` (`cargo xtask check-interfaces`).
//!
//! Each interface lives in its own inner module: the `#[zbus::proxy]` macro emits a
//! `<Signal>Args`/`<Signal>Stream` companion type per signal at module scope, and several of
//! these interfaces have a signal literally named `Changed` — nesting keeps those companion
//! types from colliding. Only the generated `<Trait>Proxy` struct is re-exported flat (as
//! before — the macro consumes the trait itself); a signal's `receive_<name>()` stream is
//! consumed through `proxy.` methods, so callers never need to name the per-module companion
//! types directly.

mod background {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Background1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Background"
    )]
    pub trait Background {
        /// `(app_id, permissions_json, approval, running, reason_json)`
        fn list(&self) -> zbus::Result<Vec<(String, String, String, bool, String)>>;

        fn grant(&self, app_id: &str) -> zbus::Result<()>;

        fn revoke(&self, app_id: &str) -> zbus::Result<()>;

        #[zbus(signal)]
        fn changed(&self, app_id: String) -> zbus::Result<()>;
    }
}

mod app_identity {
    use zbus::proxy;

    /// `(app_id, display_name, desktop_file_id, icon, categories, keywords, no_display)` —
    /// the wire shape of `AppIdentity1.ListApplications`.
    pub type ApplicationRow = (
        String,
        String,
        String,
        String,
        Vec<String>,
        Vec<String>,
        bool,
    );

    /// `(logical_app_id, display_name, icon, variant_count, default_variant)` — the wire shape of
    /// `AppIdentity1.ListLogicalApps`.
    pub type LogicalAppRow = (String, String, String, u32, String);

    /// `(variant_app_id, display_name, provenance_json)` — the wire shape of
    /// `AppIdentity1.ListVariants`. The JSON matches `interfaces/schemas/app-variant.schema.json`.
    pub type VariantRow = (String, String, String);

    /// `(instance_id, variant_app_id, logical_app_id, systemd_unit, control_group)` — the wire
    /// shape of `AppIdentity1.ListInstancesDetailed`.
    pub type InstanceDetailRow = (String, String, String, String, String);

    #[proxy(
        interface = "org.legeos.AppIdentity1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/AppIdentity"
    )]
    pub trait AppIdentity {
        /// `(canonical_app_id, instance_id, confidence, claim_matches_manifest)`
        fn resolve_window(
            &self,
            pid: u32,
            wayland_app_id: &str,
            startup_wm_class: &str,
            sandbox_app_id: &str,
        ) -> zbus::Result<(String, String, u8, bool)>;

        fn list_instances(&self) -> zbus::Result<Vec<(String, String, String, String)>>;

        fn list_apps(&self) -> zbus::Result<Vec<String>>;

        /// One [`ApplicationRow`] per installed manifest. Every id there names one *variant*:
        /// one installed copy, from one origin.
        fn list_applications(&self) -> zbus::Result<Vec<ApplicationRow>>;

        /// One [`LogicalAppRow`] per application a person would name — one Firefox, however many
        /// copies of it are installed.
        fn list_logical_apps(&self) -> zbus::Result<Vec<LogicalAppRow>>;

        /// The copies of one logical application, default first.
        fn list_variants(&self, logical_app_id: &str) -> zbus::Result<Vec<VariantRow>>;

        /// Which logical application a variant is a copy of; empty if it is not installed.
        fn logical_app_of(&self, variant_app_id: &str) -> zbus::Result<String>;

        /// One [`InstanceDetailRow`] per live instance.
        fn list_instances_detailed(&self) -> zbus::Result<Vec<InstanceDetailRow>>;

        /// The installed set changed; re-read the listings.
        #[zbus(signal)]
        fn applications_changed(&self) -> zbus::Result<()>;
    }
}

mod launcher {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Launcher1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Launcher"
    )]
    pub trait Launcher {
        /// Launch one variant — one particular installed copy.
        fn launch(
            &self,
            app_id: &str,
            arguments: Vec<String>,
            activation_token: &str,
        ) -> zbus::Result<String>;

        /// Launch a logical application, letting the daemon choose which copy. Pass an empty
        /// `preferred_variant` to mean "whichever is the default".
        fn launch_logical(
            &self,
            logical_app_id: &str,
            preferred_variant: &str,
            arguments: Vec<String>,
            activation_token: &str,
        ) -> zbus::Result<String>;
    }
}

mod associations {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Associations1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Associations"
    )]
    pub trait Associations {
        /// `(canonical_app_id, detected_mime, reason)`
        fn resolve_file(
            &self,
            path: &str,
            project_id: &str,
        ) -> zbus::Result<(String, String, String)>;

        /// `(canonical_app_id, detected_mime, reason)`
        fn resolve_uri(
            &self,
            uri: &str,
            project_id: &str,
        ) -> zbus::Result<(String, String, String)>;

        fn set_mime_default(&self, mime: &str, app_id: &str) -> zbus::Result<()>;

        fn set_extension_default(
            &self,
            extension: &str,
            app_id: &str,
            override_specific_mime: bool,
        ) -> zbus::Result<()>;

        fn set_project_default(
            &self,
            project_id: &str,
            mime: &str,
            app_id: &str,
        ) -> zbus::Result<()>;

        fn list_mime_defaults(&self) -> zbus::Result<Vec<(String, String)>>;

        fn explain(&self, path: &str, project_id: &str) -> zbus::Result<String>;

        fn forget(&self, scope: &str, key: &str, project_id: &str) -> zbus::Result<bool>;

        #[zbus(signal)]
        fn defaults_changed(&self, scope: String, key: String) -> zbus::Result<()>;
    }
}

mod providers {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Providers1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Providers"
    )]
    pub trait Providers {
        /// `(role, resolution-json)`
        fn report(&self) -> zbus::Result<Vec<(String, String)>>;

        /// `(app_id, priority)`, best first — the choice for one role.
        fn candidates(&self, role: &str) -> zbus::Result<Vec<(String, u32)>>;

        fn select(&self, role: &str, app_id: &str) -> zbus::Result<()>;

        /// Drop the explicit selection and let priority decide again.
        fn deselect(&self, role: &str) -> zbus::Result<()>;

        fn declare(&self, app_id: &str, role: &str, priority: u32) -> zbus::Result<()>;

        fn undeclare(&self, app_id: &str) -> zbus::Result<()>;

        #[zbus(signal)]
        fn changed(&self, role: String) -> zbus::Result<()>;
    }
}

mod history {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.History1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/History"
    )]
    pub trait History {
        fn recent(&self, limit: u32) -> zbus::Result<Vec<String>>;

        fn search(&self, query: &str, limit: u32) -> zbus::Result<Vec<String>>;

        #[allow(clippy::too_many_arguments)]
        fn append(
            &self,
            kind: &str,
            app_id: &str,
            instance_id: &str,
            message: &str,
            cause: &str,
            actions_json: &str,
            reversible: bool,
        ) -> zbus::Result<String>;

        fn undo(&self, event_id: &str) -> zbus::Result<bool>;

        #[zbus(signal)]
        fn changed(&self) -> zbus::Result<()>;
    }
}

mod settings {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Settings1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Settings"
    )]
    pub trait Settings {
        /// `(key, description, default-json, privileged)`
        fn schema(&self) -> zbus::Result<Vec<(String, String, String, bool)>>;

        /// Complete `lege_session_core::settings::SettingSpec` JSON documents.
        fn schema_details(&self) -> zbus::Result<Vec<String>>;

        fn get(&self, key: &str) -> zbus::Result<String>;

        fn set(&self, key: &str, value_json: &str, origin: &str) -> zbus::Result<()>;

        fn list(&self) -> zbus::Result<Vec<(String, String)>>;

        fn changes(&self, limit: u32) -> zbus::Result<Vec<String>>;

        fn reset(&self, key: &str, origin: &str) -> zbus::Result<()>;

        #[zbus(signal)]
        fn changed(&self, key: String) -> zbus::Result<()>;
    }
}

mod projects {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Projects1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Projects"
    )]
    pub trait Projects {
        fn create(&self, project_id: &str, name: &str, root: &str) -> zbus::Result<()>;

        /// `(project_id, name, root, is_open)`
        fn list(&self) -> zbus::Result<Vec<(String, String, String, bool)>>;

        fn open(&self, project_id: &str) -> zbus::Result<()>;

        fn close(&self, project_id: &str) -> zbus::Result<()>;

        fn add_app(
            &self,
            project_id: &str,
            app_id: &str,
            arguments: Vec<String>,
        ) -> zbus::Result<()>;

        fn remove_app(&self, project_id: &str, app_id: &str) -> zbus::Result<()>;

        fn remove(&self, project_id: &str) -> zbus::Result<()>;

        /// The whole project as JSON — `interfaces/schemas/project.schema.json`.
        fn describe(&self, project_id: &str) -> zbus::Result<String>;

        /// `(project_id, project_json)` for every project, in one round-trip.
        fn list_detailed(&self) -> zbus::Result<Vec<(String, String)>>;

        /// Merge a partial project object. Only the user-editable keys are accepted; an unknown
        /// key is refused rather than dropped.
        fn update(&self, project_id: &str, changes_json: &str) -> zbus::Result<()>;

        /// The project the session is working in, or `""`.
        fn get_current(&self) -> zbus::Result<String>;

        /// Set (or clear, with `""`) the current project.
        fn set_current(&self, project_id: &str) -> zbus::Result<()>;

        /// `"active"` or `"archived"`. Archived stays searchable and openable.
        fn set_state(&self, project_id: &str, state: &str) -> zbus::Result<()>;

        /// Remember a logical application rather than one copy of it. Pass an empty
        /// `preferred_variant` to mean "whichever copy is default when the project opens".
        fn add_logical_app(
            &self,
            project_id: &str,
            logical_app_id: &str,
            preferred_variant: &str,
            arguments: Vec<String>,
        ) -> zbus::Result<()>;

        /// This project changed; re-read it with `Describe`.
        #[zbus(signal)]
        fn changed(&self, project_id: String) -> zbus::Result<()>;

        /// The current project changed (`""` when cleared).
        #[zbus(signal)]
        fn current_changed(&self, project_id: String) -> zbus::Result<()>;
    }
}

mod homefield {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.HomeField1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/HomeField"
    )]
    pub trait HomeField {
        /// The document for `output` as JSON (`lege_field::FieldDocument`), created empty at
        /// `cols`×`rows` if none is stored yet.
        fn get(&self, output: &str, cols: u16, rows: u16) -> zbus::Result<String>;

        /// Apply one `lege_field::FieldOp` (JSON); returns the new revision.
        fn apply(&self, output: &str, op_json: &str) -> zbus::Result<u64>;

        /// Undo the most recent op; returns the document afterwards, or `""` if nothing to undo.
        fn undo(&self, output: &str) -> zbus::Result<String>;

        /// Discard the stored document and its undo log; returns a fresh empty one at
        /// `cols`×`rows`, which the shell's next merge fills with the default layout.
        fn reset(&self, output: &str, cols: u16, rows: u16) -> zbus::Result<String>;

        fn get_revision(&self, output: &str) -> zbus::Result<u64>;

        #[zbus(signal)]
        fn changed(&self, output: String, revision: u64) -> zbus::Result<()>;
    }
}

mod power {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Power1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Power"
    )]
    pub trait Power {
        fn get_profile(&self, source: &str) -> zbus::Result<String>;

        fn set_profile(&self, source: &str, profile_json: &str) -> zbus::Result<()>;

        fn reset_profile(&self, source: &str) -> zbus::Result<()>;
    }
}

mod notifications {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Notifications1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Notifications"
    )]
    pub trait Notifications {
        /// Active notification documents, newest first, encoded as JSON.
        fn list_active(&self) -> zbus::Result<Vec<String>>;

        fn dismiss(&self, id: u32) -> zbus::Result<bool>;

        fn clear_all(&self) -> zbus::Result<u32>;

        /// Invoke one declared action; the server emits the freedesktop `ActionInvoked` and
        /// closes the notification.
        fn invoke(&self, id: u32, action_key: &str) -> zbus::Result<bool>;

        #[zbus(signal)]
        fn changed(&self) -> zbus::Result<()>;
    }
}

mod lifecycle {
    use zbus::proxy;

    /// `org.legeos.Session.Lifecycle1` — ending, suspending and locking the session.
    ///
    /// The `Can*` methods return logind's own `"yes" | "no" | "na" | "challenge"` rather than a
    /// boolean, so a caller can tell "this machine cannot" from "this would need
    /// authentication".
    #[proxy(
        interface = "org.legeos.Session.Lifecycle1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Lifecycle"
    )]
    pub trait Lifecycle {
        fn logout(&self) -> zbus::Result<()>;

        fn reboot(&self) -> zbus::Result<()>;

        fn power_off(&self) -> zbus::Result<()>;

        fn suspend(&self) -> zbus::Result<()>;

        fn lock(&self) -> zbus::Result<()>;

        fn can_reboot(&self) -> zbus::Result<String>;

        fn can_power_off(&self) -> zbus::Result<String>;

        fn can_suspend(&self) -> zbus::Result<String>;
    }
}

mod audio {
    use zbus::proxy;

    /// `org.legeos.Audio1` — the default sink's volume.
    ///
    /// `Adjust`/`Toggle` rather than read-modify-write by the caller: the service knows where the
    /// volume was, so two clients on the same volume key cannot each undo the other.
    #[proxy(
        interface = "org.legeos.Audio1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Audio"
    )]
    pub trait Audio {
        /// `AudioState` JSON: `{ present, volume_percent, muted }`.
        fn get(&self) -> zbus::Result<String>;

        fn set_volume(&self, percent: u32) -> zbus::Result<()>;

        /// Step the volume and report where it landed.
        fn adjust_volume(&self, delta: i32) -> zbus::Result<u32>;

        fn set_muted(&self, muted: bool) -> zbus::Result<()>;

        /// Flip mute and report the new state.
        fn toggle_muted(&self) -> zbus::Result<bool>;

        /// This service changed the volume or the mute state. Carries nothing — call `get`.
        #[zbus(signal)]
        fn changed(&self) -> zbus::Result<()>;
    }
}

mod backlight {
    use zbus::proxy;

    /// `org.legeos.Backlight1` — the display backlight.
    #[proxy(
        interface = "org.legeos.Backlight1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Backlight"
    )]
    pub trait Backlight {
        /// `BacklightState` JSON: `{ present, percent, device, raw, max_raw }`.
        fn get(&self) -> zbus::Result<String>;

        fn set_brightness(&self, percent: u32) -> zbus::Result<()>;

        /// Step the backlight and report where it landed.
        fn adjust_brightness(&self, delta: i32) -> zbus::Result<u32>;

        /// This service changed the backlight. Carries nothing — call `get`.
        #[zbus(signal)]
        fn changed(&self) -> zbus::Result<()>;
    }
}

mod battery {
    use zbus::proxy;

    /// `org.legeos.Battery1` — what the battery is doing. Read-only by construction.
    #[proxy(
        interface = "org.legeos.Battery1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Battery"
    )]
    pub trait Battery {
        /// `BatteryState` JSON: `{ present, percent, status, on_mains, time_to_empty_minutes,
        /// time_to_full_minutes }`.
        fn get(&self) -> zbus::Result<String>;
    }
}

mod location {
    use zbus::proxy;

    #[proxy(
        interface = "org.legeos.Location1",
        default_service = "org.legeos.Session",
        default_path = "/org/legeos/Session/Location"
    )]
    pub trait Location {
        fn decide(&self, location_key: &str, candidates_json: &str) -> zbus::Result<String>;

        fn set_manual_override(&self, location_key: &str, timezone: &str) -> zbus::Result<()>;

        fn clear_manual_override(&self, location_key: &str) -> zbus::Result<()>;

        fn manual_overrides(&self) -> zbus::Result<Vec<(String, String)>>;
    }
}

pub use app_identity::{
    AppIdentityProxy, ApplicationRow, InstanceDetailRow, LogicalAppRow, VariantRow,
};
pub use associations::AssociationsProxy;
pub use audio::AudioProxy;
pub use background::BackgroundProxy;
pub use backlight::BacklightProxy;
pub use battery::BatteryProxy;
pub use history::HistoryProxy;
pub use homefield::HomeFieldProxy;
pub use launcher::LauncherProxy;
pub use lifecycle::LifecycleProxy;
pub use location::LocationProxy;
pub use notifications::NotificationsProxy;
pub use power::PowerProxy;
pub use projects::ProjectsProxy;
pub use providers::ProvidersProxy;
pub use settings::SettingsProxy;
