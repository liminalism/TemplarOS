//! Explicit user-child scopes owned by sessiond, separate from application identity.
#[zbus::proxy(interface = "org.templaros.Workloads1", default_service = "org.templaros.Session", default_path = "/org/templaros/Session/Workloads")]
pub trait Workloads {
    fn register_child(&self, child_pid: u32, class: &str, command_summary: &str) -> zbus::Result<String>;
    fn resolve(&self, pid: u32) -> zbus::Result<String>;
    fn list(&self) -> zbus::Result<String>;
    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
}
