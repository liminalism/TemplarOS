//! Start applications in dedicated systemd units.
//!
//! [`LaunchBackend::Systemd1`] — a direct `org.freedesktop.systemd1.Manager.StartTransientUnit()`
//! call (`crate::systemd1`) — is the backend that ships. The launcher reads the manager's actual
//! ControlGroup before registering the instance or publishing its launch event; it never derives
//! a guessed path from the unit name.
//! [`LaunchBackend::SystemdRun`] (the `systemd-run --user` subprocess adapter) is kept as the
//! fallback for a session bus where systemd does not answer; [`LaunchBackend::detect`] picks
//! automatically. Neither ever runs `Exec=` through a shell: the registered executable plus an
//! argument vector.

use crate::identity::{InstanceRecord, Registry};
use crate::systemd1;
use anyhow::{Context, Result, bail};
use std::{ffi::OsString, path::{Path, PathBuf}, sync::Arc, time::Duration};
use templar_client::proxies::ResourcesProxy;
use templar_types::{AppManifest, InstanceId, ResourceClass, dbus::env, unit};
use tokio::{process::Command, time::sleep};

/// How the launcher starts processes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LaunchBackend {
    /// `StartTransientUnit()` on the session bus. Ships as the default.
    Systemd1,
    /// `systemd-run --user`. Fallback when systemd does not answer on the session bus.
    #[default]
    SystemdRun,
}

impl LaunchBackend {
    /// Probe the session bus once at startup and pick the backend to use for the rest of the
    /// process's life. `None` (no session bus, e.g. `--dry-run`) always yields `SystemdRun`,
    /// which is never actually invoked in that mode either.
    pub async fn detect(connection: Option<&zbus::Connection>) -> Self {
        match connection {
            Some(connection) if systemd1::is_available(connection).await => Self::Systemd1,
            _ => Self::SystemdRun,
        }
    }
}

/// Launch `manifest` with `arguments` and register the resulting instance.
pub async fn launch(
    registry: Arc<Registry>,
    manifest: Arc<AppManifest>,
    arguments: &[String],
    activation_token: Option<&str>,
    backend: LaunchBackend,
    connection: Option<&zbus::Connection>,
) -> Result<InstanceRecord> {
    launch_in_directory(registry, manifest, arguments, activation_token, backend, connection, None).await
}

/// Same brokered launch with a real service cwd, independent of any application's CLI spelling.
pub async fn launch_in_directory(
    registry: Arc<Registry>,
    manifest: Arc<AppManifest>,
    arguments: &[String],
    activation_token: Option<&str>,
    backend: LaunchBackend,
    connection: Option<&zbus::Connection>,
    directory: Option<&std::path::Path>,
) -> Result<InstanceRecord> {
    let arguments = explicit_arguments(&registry, &manifest, arguments);
    launch_native(registry, manifest, arguments, activation_token, backend, connection, directory).await
}

/// Desktop file targets are not arbitrary application switches. Field codes decide whether
/// each validated local target reaches the application as native pathname bytes or as a URI.
pub async fn launch_files(
    registry: Arc<Registry>,
    manifest: Arc<AppManifest>,
    file_uris: &[String],
    activation_token: Option<&str>,
    backend: LaunchBackend,
    connection: Option<&zbus::Connection>,
) -> Result<InstanceRecord> {
    let arguments = file_arguments(&registry, &manifest, file_uris)?;
    launch_native(registry, manifest, arguments, activation_token, backend, connection, None).await
}

fn file_arguments(registry: &Registry, manifest: &AppManifest, file_uris: &[String]) -> Result<Vec<OsString>> {
    anyhow::ensure!(!file_uris.is_empty(), "file launch requires at least one local target");
    let targets: Vec<_> = file_uris.iter().map(|uri| {
        let path = templar_types::uri_list::decode_uri(uri).context("file launch requires a local file URI")?;
        anyhow::ensure!(path.is_absolute() && path.exists(), "file launch target must exist");
        Ok(crate::desktop_entry::FileTarget { path, uri: uri.clone() })
    }).collect::<Result<_>>()?;
    Ok(if let Some(template) = registry.launch_template(&manifest.id) {
        anyhow::ensure!(template.exec.accepts_files(), "desktop entry has no file/URI field code");
        template.exec.expand_arguments(&targets, manifest.icon.as_deref().unwrap_or(""),
            &manifest.display_name, &template.desktop_file_path)
    } else {
        // Native manifests declare a resolved executable rather than a desktop Exec line.
        targets.into_iter().map(|target| target.path.into_os_string()).collect()
    })
}

fn explicit_arguments(registry: &Registry, manifest: &AppManifest, arguments: &[String]) -> Vec<OsString> {
    let mut expanded = registry.launch_template(&manifest.id).map_or_else(Vec::new, |template|
        template.exec.expand_arguments(&[], manifest.icon.as_deref().unwrap_or(""),
            &manifest.display_name, &template.desktop_file_path));
    expanded.extend(arguments.iter().map(|argument| OsString::from(argument.as_str())));
    expanded
}

/// Systemd's string transport cannot carry invalid UTF-8 argv or cwd. Only those launches
/// use the same-PID native exec adapter; UTF-8 cwd remains a real WorkingDirectory property.
fn wire_command<'a>(
    executable: &'a Path,
    arguments: Vec<OsString>,
    directory: Option<&'a Path>,
) -> Result<(&'a str, Vec<String>, Option<&'a Path>)> {
    let native_directory = directory.filter(|directory| directory.to_str().is_none());
    if let Some(executable) = executable.to_str()
        && arguments.iter().all(|argument| argument.to_str().is_some())
        && native_directory.is_none()
    {
        return Ok((executable, arguments.into_iter().map(|argument| argument.into_string().expect("validated UTF-8")).collect(), directory));
    }
    Ok((crate::native_exec::ADAPTER_PATH,
        vec![crate::native_exec::encode(executable, &arguments, native_directory)?],
        directory.filter(|directory| directory.to_str().is_some())))
}

async fn launch_native(
    registry: Arc<Registry>,
    manifest: Arc<AppManifest>,
    arguments: Vec<OsString>,
    activation_token: Option<&str>,
    backend: LaunchBackend,
    connection: Option<&zbus::Connection>,
    directory: Option<&Path>,
) -> Result<InstanceRecord> {
    if let Some(directory) = directory {
        anyhow::ensure!(directory.is_absolute() && directory.is_dir(), "launch directory must be an existing absolute directory");
    }
    let (executable, arguments, service_directory) = wire_command(&manifest.executable, arguments, directory)?;
    if executable == crate::native_exec::ADAPTER_PATH {
        anyhow::ensure!(Path::new(executable).is_file(), "native exec adapter is not installed at {executable}");
    }
    let instance_id = InstanceId::new();
    let record = match (backend, connection) {
        (LaunchBackend::Systemd1, Some(connection)) => systemd1::start_transient_unit_in_directory(
            connection,
            &manifest,
            instance_id,
            executable,
            &arguments,
            activation_token,
            service_directory,
        )
        .await
        .context("launching via StartTransientUnit")?,
        _ => launch_via_systemd_run(&manifest, instance_id, executable, &arguments, activation_token, service_directory).await?,
    };
    anyhow::ensure!(record.has_managed_identity(), "systemd returned an inconsistent managed launch identity");
    registry.register_instance(record.clone());
    if let Some(connection) = connection {
        push_resource_hints(connection, instance_id, &manifest).await;
    }
    Ok(record)
}

/// Best-effort: tell `templar-resourced` this instance's manifest-derived protection before the
/// governor ever needs to classify it (`org.templaros.Resources1.SetInstanceHints`). Not fatal —
/// the governor treats an instance it has no hints for as an ordinary, unprotected one, and will
/// get these same hints the next time this instance's checkpoint state changes.
async fn push_resource_hints(
    connection: &zbus::Connection,
    instance_id: InstanceId,
    manifest: &AppManifest,
) {
    let Ok(proxy) = ResourcesProxy::new(connection).await else {
        return;
    };
    let protected = manifest.resource_class == ResourceClass::SessionCritical;
    let disposable = manifest.resource_class == ResourceClass::Disposable;
    if let Err(error) = proxy
        .set_instance_hints(
            &instance_id.to_string(),
            protected,
            disposable,
            manifest.supports_checkpoint,
            false,
        )
        .await
    {
        tracing::debug!(%error, %instance_id, "could not push resource hints (templar-resourced may not be running yet)");
    }
}

async fn launch_via_systemd_run(
    manifest: &AppManifest,
    instance_id: InstanceId,
    executable: &str,
    arguments: &[String],
    activation_token: Option<&str>,
    directory: Option<&std::path::Path>,
) -> Result<InstanceRecord> {
    let unit_name = unit::unit_name(&manifest.id, instance_id);
    let mut command = Command::new("systemd-run");
    command
        .arg("--user")
        .arg("--collect")
        .arg("--quiet")
        .arg("--service-type=exec")
        .arg("--expand-environment=no")
        .arg(format!("--unit={unit_name}"))
        .arg("--property=Slice=app.slice")
        .arg("--property=MemoryAccounting=yes")
        .arg("--property=IOAccounting=yes")
        .arg("--property=TasksAccounting=yes")
        // Whole-application termination if the kernel OOM killer ever fires inside the group.
        .arg("--property=OOMPolicy=kill")
        // See `systemd1::transient_unit_properties` for why both of these are set rather than
        // inherited: an application that dies on startup must leave a record.
        .arg("--property=StandardOutput=journal")
        .arg("--property=StandardError=journal")
        .arg(format!("--setenv={}={instance_id}", env::INSTANCE_ID))
        .arg(format!("--setenv={}={}", env::APP_ID, manifest.id));

    if let Some(directory) = directory {
        command.arg(format!("--working-directory={}", directory.to_str().context("launch directory must be UTF-8")?));
    }
    // Which desktop this application joins is stated here, not left to the systemd user manager's
    // global environment — `systemd1::SESSION_ENVIRONMENT` has the full reasoning.
    for assignment in crate::systemd1::session_environment() {
        command.arg(format!("--setenv={assignment}"));
    }

    if let Some(token) = activation_token {
        command.arg(format!("--setenv=XDG_ACTIVATION_TOKEN={token}"));
    }

    // Never run `Exec=` through a shell: the registered executable plus an argument vector.
    command.arg("--").arg(executable).args(arguments);

    let status = command
        .status()
        .await
        .context("starting application through systemd-run")?;
    if !status.success() {
        bail!("systemd-run failed with {status}");
    }

    let control_group = wait_for_control_group(&unit_name).await?;

    Ok(InstanceRecord {
        id: instance_id,
        app_id: manifest.id.clone(),
        systemd_unit: unit_name,
        control_group,
    })
}

/// A successful unit start does not authorize a guessed cgroup. Report unavailable identity
/// explicitly if the user manager never exposes its actual ControlGroup.
async fn wait_for_control_group(unit: &str) -> Result<PathBuf> {
    for _ in 0..100 {
        if let Ok(output) = Command::new("systemctl")
            .args(["--user", "show", unit, "--property=ControlGroup", "--value"])
            .output()
            .await
            && output.status.success()
            && let Ok(value) = String::from_utf8(output.stdout)
        {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(PathBuf::from(value));
            }
        }

        sleep(Duration::from_millis(10)).await;
    }

    bail!("systemd accepted {unit} but did not expose its actual ControlGroup");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_entry::ExecTemplate;
    use std::os::unix::ffi::OsStringExt as _;
    use templar_types::AppAliases;

    fn manifest() -> AppManifest {
        AppManifest {
            id: "org.example.Reader".parse().unwrap(), display_name: "Reader".into(),
            primary_desktop_file_id: "reader.desktop".into(), executable: "/usr/bin/reader".into(),
            aliases: AppAliases::default(), mime_types: Default::default(),
            resource_class: ResourceClass::Normal, supports_checkpoint: false, icon: None,
            categories: Default::default(), keywords: Default::default(), no_display: false,
            provenance: None, provides: Vec::new(),
        }
    }

    #[test]
    fn explicit_launch_keeps_desktop_fixed_arguments_once_without_guessing_file_targets() {
        let registry = Registry::default();
        let manifest = manifest();
        registry.insert_desktop_manifest(manifest.clone(), ExecTemplate::parse("reader --reader %F"),
            "/usr/share/applications/reader.desktop".into());
        let arguments = explicit_arguments(&registry, &manifest, &["--directory".into(), "/tmp/project".into()]);
        assert_eq!(arguments, ["--reader", "--directory", "/tmp/project"]);
        let (executable, wire, directory) = wire_command(&manifest.executable, arguments, None).unwrap();
        assert!(directory.is_none());
        assert_eq!(executable, "/usr/bin/reader");
        assert_eq!(wire, ["--reader", "--directory", "/tmp/project"]);
    }

    #[test]
    fn native_cwd_round_trip_keeps_flatpak_dispatch_and_literal_fixed_arguments() {
        let registry = Registry::default();
        let mut manifest = manifest();
        manifest.id = "flatpak.org.example.Reader".parse().unwrap();
        manifest.executable = "/usr/bin/flatpak".into();
        registry.insert_desktop_manifest(manifest.clone(),
            ExecTemplate::parse("flatpak run --branch=stable org.example.Reader --fixed=${HOME}:$VAR:$$ %F"),
            "/var/lib/flatpak/exports/share/applications/org.example.Reader.desktop".into());
        let arguments = explicit_arguments(&registry, &manifest, &["two words".into()]);
        assert_eq!(arguments, ["run", "--branch=stable", "org.example.Reader", "--fixed=${HOME}:$VAR:$$", "two words"]);
        let utf8_directory = Path::new("/tmp/project-${HOME}-$VAR-$$");
        let (executable, wire, directory) = wire_command(&manifest.executable, arguments.clone(), Some(utf8_directory)).unwrap();
        assert_eq!(executable, "/usr/bin/flatpak");
        assert_eq!(wire, ["run", "--branch=stable", "org.example.Reader", "--fixed=${HOME}:$VAR:$$", "two words"]);
        assert_eq!(directory, Some(utf8_directory));
        let native_directory = PathBuf::from(OsString::from_vec(b"/tmp/project-\xff".to_vec()));
        let (executable, wire, directory) = wire_command(&manifest.executable, arguments.clone(), Some(&native_directory)).unwrap();
        assert_eq!(executable, crate::native_exec::ADAPTER_PATH);
        assert!(directory.is_none());
        let decoded = crate::native_exec::decode(&wire[0]).unwrap();
        assert_eq!(decoded.executable, manifest.executable);
        assert_eq!(decoded.arguments, arguments);
        assert_eq!(decoded.directory, Some(native_directory));
    }

    #[test]
    fn native_argv_keeps_utf8_cwd_as_a_real_systemd_property() {
        let manifest = manifest();
        let directory = Path::new("/tmp/project");
        let arguments = vec![OsString::from_vec(b"/tmp/file-\xff".to_vec())];
        let (_, wire, service_directory) = wire_command(&manifest.executable, arguments.clone(), Some(directory)).unwrap();
        assert_eq!(service_directory, Some(directory));
        let decoded = crate::native_exec::decode(&wire[0]).unwrap();
        assert_eq!(decoded.arguments, arguments);
        assert!(decoded.directory.is_none());
    }

    #[test]
    fn native_files_use_registered_field_codes_and_lossless_adapter_payload() {
        let temp = tempfile::tempdir().unwrap();
        let files = [
            temp.path().join(OsString::from_vec(b"name-\xff.pdf".to_vec())),
            temp.path().join(OsString::from_vec(b"name-\xfe.pdf".to_vec())),
        ];
        for file in &files { std::fs::write(file, "fixture").unwrap(); }
        let uris: Vec<_> = files.iter().map(|file| templar_types::uri_list::encode_path(file)).collect();
        let registry = Registry::default();
        let manifest = manifest();
        registry.insert_desktop_manifest(manifest.clone(), ExecTemplate::parse("reader --reader %F"),
            "/usr/share/applications/reader.desktop".into());
        let arguments = file_arguments(&registry, &manifest, &uris).unwrap();
        assert_eq!(arguments, [OsString::from("--reader"), files[0].as_os_str().to_owned(), files[1].as_os_str().to_owned()]);
        let (executable, wire, directory) = wire_command(&manifest.executable, arguments.clone(), None).unwrap();
        assert!(directory.is_none());
        assert_eq!(executable, crate::native_exec::ADAPTER_PATH);
        assert_eq!(wire.len(), 1);
        let decoded = crate::native_exec::decode(&wire[0]).unwrap();
        assert_eq!(decoded.executable, manifest.executable);
        assert_eq!(decoded.arguments, arguments);
        assert!(decoded.directory.is_none());

        registry.insert_desktop_manifest(manifest.clone(), ExecTemplate::parse("reader --uri %U"),
            "/usr/share/applications/reader.desktop".into());
        let arguments = file_arguments(&registry, &manifest, &uris).unwrap();
        let (executable, wire, _) = wire_command(&manifest.executable, arguments, None).unwrap();
        assert_eq!(executable, "/usr/bin/reader");
        assert_eq!(wire, ["--uri".to_owned(), uris[0].clone(), uris[1].clone()]);
        assert!(file_arguments(&registry, &manifest, &["file://remote/tmp/a.pdf".into()]).is_err());
        registry.insert_desktop_manifest(manifest.clone(), ExecTemplate::parse("reader --reader"),
            "/usr/share/applications/reader.desktop".into());
        assert!(file_arguments(&registry, &manifest, &uris).is_err());
    }
}
