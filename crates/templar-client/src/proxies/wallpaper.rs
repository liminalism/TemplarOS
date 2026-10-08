//! Native wallpaper policy is hosted by sessiond; pixels remain compositor-owned.
use zbus::proxy;

#[proxy(interface = "org.templaros.Wallpaper1", default_service = "org.templaros.Session", default_path = "/org/templaros/Wallpaper")]
pub trait Wallpaper {
    fn inspect(&self) -> zbus::Result<String>;
    fn next(&self) -> zbus::Result<String>;
    fn previous(&self) -> zbus::Result<String>;
    fn pause(&self) -> zbus::Result<String>;
    fn resume(&self) -> zbus::Result<String>;
    fn refresh(&self) -> zbus::Result<String>;
    #[zbus(signal)]
    fn changed(&self, json: &str) -> zbus::Result<()>;
}
