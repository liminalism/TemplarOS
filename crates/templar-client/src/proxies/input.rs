//! Per-device native input preferences, with compositor-owner revision fences.
use zbus::proxy;
#[proxy(
    interface = "org.templaros.Input1",
    default_service = "org.templaros.Session",
    default_path = "/org/templaros/Session/Input"
)]
pub trait Input {
    fn observe(&self) -> zbus::Result<String>;
    fn configure(&self, expected: &str, id: &str, configuration: &str) -> zbus::Result<String>;
    fn reset(&self, expected: &str, id: &str) -> zbus::Result<String>;
    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
}
