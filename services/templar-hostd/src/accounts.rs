//! Unix-FD password transport and hostd-owned, recoverable maintenance.
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
};
use zbus::{message::Header, zvariant::OwnedFd};

#[derive(Debug)]
pub struct AccountsApi(pub Arc<crate::state::HostdState>);

#[zbus::interface(name = "org.templaros.Host.Accounts1")]
impl AccountsApi {
    async fn change_self_password(
        &self,
        secret: OwnedFd,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("missing caller identity".into()))?;
        let bus = zbus::fdo::DBusProxy::new(connection).await?;
        let uid = bus.get_connection_unix_user(sender.clone().into()).await?;
        if uid < 1000 {
            return Err(zbus::fdo::Error::AccessDenied(
                "only a local user's own password may change; do not use sudo".into(),
            ));
        }
        let action = "org.templaros.host.account";
        if !self
            .0
            .polkit
            .check_authorization_interactive(sender.as_str(), action)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
        {
            return Err(zbus::fdo::Error::AccessDenied(
                "self-password authentication was not granted".into(),
            ));
        }
        // Re-resolve after authentication: an exited caller is never inherited by another name.
        if bus.get_connection_unix_user(sender.clone().into()).await? != uid {
            return Err(zbus::fdo::Error::AccessDenied(
                "caller identity changed".into(),
            ));
        }
        let state = self.0.clone();
        let descriptor: std::os::fd::OwnedFd = secret.into();
        tokio::task::spawn_blocking(move || {
            change_password(&state, uid, fs::File::from(descriptor))
        })
        .await
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }
}

fn arguments(root: &Path, state: &Path) -> Vec<String> {
    vec![
        "--mount".into(),
        "--propagation".into(),
        "private".into(),
        "/usr/bin/python3".into(),
        "-I".into(),
        "/usr/libexec/templaros-account-maintenance".into(),
        "--root".into(),
        root.display().to_string(),
        "--state".into(),
        state.display().to_string(),
    ]
}

pub fn recover(state: &crate::state::HostdState) -> Result<()> {
    let directory = state.apt_root.join("var/lib/templaros/hostd/accounts");
    if !directory.join("intent.json").exists()
        && !state
            .apt_root
            .join("var/lib/templaros/account-maintenance/active.json")
            .exists()
    {
        return Ok(());
    }
    let mut args = arguments(&state.apt_root, &directory);
    args.push("--recover".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = state.commands.run("/usr/bin/unshare", &args)?;
    if !output.status.success() {
        bail!(
            "account recovery failed; refusing updates/credential mutation until recovered: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn change_password(state: &crate::state::HostdState, uid: u32, secret: fs::File) -> Result<()> {
    state.engine.with_maintenance(|| {
        recover(state)?;
        let mut roots = BTreeSet::new();
        for deployment in state.deployment_registry().deployments {
            if !crate::layout::safe_subvolume(&deployment.subvolume) {
                bail!("unsafe legacy deployment identity; reviewed migration required");
            }
            roots.insert(deployment.subvolume);
        }
        if let Some(candidate) = state.engine.candidate_deployment() {
            roots.insert(candidate.subvolume);
        }
        let directory = state.apt_root.join("var/lib/templaros/hostd/accounts");
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let request = directory.join("roots.json");
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&request)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            serde_json::to_writer(
                &mut file,
                &serde_json::json!({"schema": crate::persistence::SCHEMA, "subvolumes": roots}),
            )?;
            file.flush()?;
            file.sync_all()?;
        }
        crate::persistence::sync_parent(&request)?;
        let mut args = arguments(&state.apt_root, &directory);
        args.extend([
            "--uid".into(),
            uid.to_string(),
            "--roots".into(),
            request.display().to_string(),
        ]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = state
            .commands
            .run_with_stdin("/usr/bin/unshare", &args, secret);
        let outcome = match output {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => recover(state).and_then(|()| {
                Err(anyhow::anyhow!(
                    "password maintenance refused or recovered: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }),
            Err(error) => recover(state)
                .and_then(|()| Err(error).context("running owned password maintenance")),
        };
        let cleanup =
            fs::remove_file(&request).context("removing credential-free account root request");
        outcome?;
        cleanup
    })
}

/// Credentials are root-paired System, never silently rolled back with user Work.
/// Existing non-native unit fixtures have neither account database and remain valid fixtures.
pub fn ensure_compatible(source: &Path, destination: &Path) -> Result<()> {
    let native = source.join(crate::layout::INSTALLED_LAYOUT).exists();
    for name in ["passwd", "shadow", "group", "gshadow"] {
        let read = |root: &Path| -> Result<Option<Vec<u8>>> {
            match fs::read(root.join("etc").join(name)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !native => Ok(None),
                Err(error) => Err(error).context("reading root-paired account database"),
            }
        };
        if read(source)? != read(destination)? {
            bail!(
                "root-paired {name} differs; refusing credential/account downgrade or implicit package migration"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_account_changes_cannot_be_silently_rolled_back() {
        let current = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        for root in [current.path(), fallback.path()] {
            fs::create_dir_all(root.join("etc")).unwrap();
            for name in ["passwd", "shadow", "group", "gshadow"] {
                fs::write(root.join("etc").join(name), format!("paired-{name}\n")).unwrap();
            }
        }
        ensure_compatible(current.path(), fallback.path()).unwrap();
        fs::write(
            current.path().join("etc/shadow"),
            b"changed-encrypted-shadow\n",
        )
        .unwrap();
        assert!(ensure_compatible(current.path(), fallback.path()).is_err());
        assert_eq!(
            fs::read(fallback.path().join("etc/shadow")).unwrap(),
            b"paired-shadow\n"
        );
    }

    #[test]
    fn native_missing_accounts_are_not_an_empty_fixture_fallback() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let installed = source.path().join(crate::layout::INSTALLED_LAYOUT);
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(
            installed,
            include_str!("../../../distro/deploy/layout.toml"),
        )
        .unwrap();
        assert!(ensure_compatible(source.path(), target.path()).is_err());
    }
}
