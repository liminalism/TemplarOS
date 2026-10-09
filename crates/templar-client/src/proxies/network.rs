use zbus::proxy;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};

#[proxy(
    interface = "org.templaros.Network1",
    default_service = "org.templaros.Session",
    default_path = "/org/templaros/Session/Network"
)]
pub trait Network {
    fn inventory(&self) -> zbus::Result<String>;
    fn acquire_ui(&self) -> zbus::Result<()>;
    fn release_ui(&self) -> zbus::Result<()>;
    fn set_wireless(&self, enabled: bool) -> zbus::Result<()>;
    fn scan(&self, device: &str) -> zbus::Result<()>;
    fn connect(
        &self,
        device: &str,
        access_point: &str,
        ssid: &[u8],
        security: &str,
        hidden: bool,
        session_only: bool,
    ) -> zbus::Result<OwnedObjectPath>;
    fn activate(&self, profile: &str, device: &str) -> zbus::Result<OwnedObjectPath>;
    fn disconnect(&self, device: &str) -> zbus::Result<()>;
    fn cancel(&self) -> zbus::Result<()>;
    fn forget(&self, profile: &str) -> zbus::Result<()>;
    fn set_autoconnect(&self, profile: &str, enabled: bool) -> zbus::Result<()>;
    fn answer(&self, token: &str, approved: bool) -> zbus::Result<()>;
    fn answer_secret(&self, token: &str, secret: OwnedFd) -> zbus::Result<()>;
    #[zbus(signal)]
    fn changed(&self, revision_json: &str) -> zbus::Result<()>;
}
