//! Native display observations and compositor-owned previews. No GUI rollback timer.
use zbus::proxy;
#[proxy(
    interface = "org.templaros.Displays1",
    default_service = "org.templaros.Session",
    default_path = "/org/templaros/Session/Displays"
)]
pub trait Displays {
    fn observe(&self) -> zbus::Result<String>;
    fn validate(&self, expected: &str, profile: &str) -> zbus::Result<String>;
    fn preview(&self, expected: &str, profile: &str) -> zbus::Result<String>;
    fn confirm(&self, expected: &str, preview_token: &str) -> zbus::Result<String>;
    fn revert(&self, preview_token: &str) -> zbus::Result<String>;
    fn identify(&self, expected: &str, outputs: &str, calibration: bool) -> zbus::Result<String>;
    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
}
