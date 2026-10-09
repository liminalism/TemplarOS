use zbus::{proxy, zvariant::OwnedFd};
#[proxy(
    interface = "org.templaros.Bluetooth1",
    default_service = "org.templaros.Session",
    default_path = "/org/templaros/Session/Bluetooth"
)]
pub trait Bluetooth {
    fn inventory(&self) -> zbus::Result<String>;
    fn acquire_ui(&self) -> zbus::Result<()>;
    fn release_ui(&self) -> zbus::Result<()>;
    fn set_powered(&self, adapter: &str, powered: bool) -> zbus::Result<()>;
    fn discover(&self, adapter: &str, enabled: bool) -> zbus::Result<()>;
    fn pair(&self, device: &str) -> zbus::Result<()>;
    fn cancel(&self) -> zbus::Result<()>;
    fn connect(&self, device: &str) -> zbus::Result<()>;
    fn disconnect(&self, device: &str) -> zbus::Result<()>;
    fn forget(&self, device: &str) -> zbus::Result<()>;
    fn set_trusted(&self, device: &str, trusted: bool) -> zbus::Result<()>;
    fn answer(&self, token: &str, approved: bool) -> zbus::Result<()>;
    fn answer_secret(&self, token: &str, secret: OwnedFd) -> zbus::Result<()>;
    fn display_secret(&self, token: &str) -> zbus::Result<OwnedFd>;
    #[zbus(signal)]
    fn changed(&self, revision_json: &str) -> zbus::Result<()>;
}
