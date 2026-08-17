//! zbus proxies. Kept in sync with `interfaces/dbus/*.xml` (`cargo xtask check-interfaces`).

use zbus::proxy;

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
}

#[proxy(
    interface = "org.legeos.Launcher1",
    default_service = "org.legeos.Session",
    default_path = "/org/legeos/Session/Launcher"
)]
pub trait Launcher {
    fn launch(
        &self,
        app_id: &str,
        arguments: Vec<String>,
        activation_token: &str,
    ) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.legeos.Associations1",
    default_service = "org.legeos.Session",
    default_path = "/org/legeos/Session/Associations"
)]
pub trait Associations {
    /// `(canonical_app_id, detected_mime, reason)`
    fn resolve_file(&self, path: &str, project_id: &str) -> zbus::Result<(String, String, String)>;

    fn set_mime_default(&self, mime: &str, app_id: &str) -> zbus::Result<()>;

    fn list_mime_defaults(&self) -> zbus::Result<Vec<(String, String)>>;
}

#[proxy(
    interface = "org.legeos.Resources1",
    default_service = "org.legeos.Resources",
    default_path = "/org/legeos/Resources"
)]
pub trait Resources {
    /// Current pressure level (`normal|elevated|severe|critical`) and observe-only flag.
    fn status(&self) -> zbus::Result<(String, bool)>;
}
