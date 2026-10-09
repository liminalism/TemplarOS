//! Consumer-visible native regression: actual registry, SQL abort, private bus and exported file.
use super::*;
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use templar_associationd::{
    dbus::AssociationApi, detector::SharedMimeInfoDetector, engine::AssociationEngine,
};
use tokio::io::AsyncBufReadExt;

#[tokio::test]
async fn native_catalogue_export_matches_committed_identity() -> Result<()> {
    let mut daemon = tokio::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines =
        tokio::io::BufReader::new(daemon.stdout.take().context("private bus stdout")?).lines();
    let address = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await??
        .context("private Unix bus address")?;
    ensure!(address.starts_with("unix:"), "not an owned Unix bus");
    let server = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;
    let client = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await?;
    let root = tempfile::tempdir()?;
    let apps = root.path().join("applications");
    std::fs::create_dir(&apps)?;
    let old_desktop = apps.join("org.example.Old.desktop");
    let new_desktop = apps.join("org.example.New.desktop");
    std::fs::write(
        &old_desktop,
        "[Desktop Entry]\nType=Application\nName=Owned handler\nExec=/usr/bin/true\nMimeType=application/pdf;\n",
    )?;
    let registry = Arc::new(Registry::default());
    let store = Store::in_memory()?;
    let resolver = templar_appd::desktop_entry::PathExecResolver::from_env();
    super::super::desktop_import::import(&registry, &store, &[apps.clone()], &resolver)?;
    super::super::desktop_import::regroup_and_persist(&registry, &store)?;
    let old = "desktop-import.org.example.Old.desktop".parse()?;
    let new = "desktop-import.org.example.New.desktop".parse()?;
    store.transaction(|tx| {
        templar_associationd::store::set_mime_default_within(
            tx,
            "application/pdf",
            &old,
            registry.logical_of(&old).as_ref(),
        )
    })?;
    let engine = Arc::new(RwLock::new(AssociationEngine::new(
        templar_associationd::store::load_db(&store)?,
        registry.clone(),
    )));
    let export = root.path().join("mimeapps.list");
    server
        .object_server()
        .at(
            templar_types::dbus::session::APP_IDENTITY_PATH,
            templar_appd::dbus::IdentityApi::new(registry.clone()),
        )
        .await?;
    server
        .object_server()
        .at(
            templar_types::dbus::session::ASSOCIATIONS_PATH,
            AssociationApi::new(
                engine.clone(),
                Arc::new(SharedMimeInfoDetector::load(root.path())?),
                store.clone(),
                registry.clone(),
                export.clone(),
            ),
        )
        .await?;
    ensure!(
        std::fs::read_to_string(&export)?.contains("application/pdf=org.example.Old.desktop;"),
        "startup did not export actual registered desktop ID"
    );
    let proxy = zbus::Proxy::new(
        &client,
        server
            .unique_name()
            .context("serving unique name")?
            .as_str(),
        templar_types::dbus::session::APP_IDENTITY_PATH,
        "org.templaros.AppIdentity1",
    )
    .await?;
    let mut changes = proxy.receive_signal("ApplicationsChanged").await?;
    std::fs::rename(&old_desktop, &new_desktop)?;
    store.transaction(|tx| {
        templar_appd::store::save_variant_alias_within(
            tx,
            &old,
            &new,
            "native catalogue regression",
        )
    })?;
    super::super::desktop_import::import(&registry, &store, &[apps.clone()], &resolver)?;
    super::super::desktop_import::regroup_and_persist(&registry, &store)?;
    announce_applications_changed(&server).await;
    tokio::time::timeout(Duration::from_secs(2), changes.next())
        .await?
        .context("alias catalogue signal")?;
    ensure!(
        std::fs::read_to_string(&export)?.contains("application/pdf=org.example.New.desktop;")
            && !std::fs::read_to_string(&export)?
                .contains("application/pdf=org.example.Old.desktop;"),
        "consumer catalogue signal preceded canonical external export"
    );
    let pdf = root.path().join("owned.pdf");
    std::fs::write(&pdf, b"%PDF-1.7\n")?;
    let association = zbus::Proxy::new(
        &client,
        server.unique_name().unwrap().as_str(),
        templar_types::dbus::session::ASSOCIATIONS_PATH,
        "org.templaros.Associations1",
    )
    .await?;
    let resolved: (String, String, String) = association
        .call("ResolveFile", &(pdf.to_str().unwrap(), ""))
        .await?;
    ensure!(
        resolved.0 == new.as_str() && resolved.2 == "variant-substituted",
        "owned and external consumers disagreed about alias substitution"
    );

    let committed = registry.logical_index().rows();
    std::fs::write(
        apps.join("org.example.Distinct.desktop"),
        "[Desktop Entry]\nType=Application\nName=Distinct\nExec=/usr/bin/false\n",
    )?;
    super::super::desktop_import::import(&registry, &store, &[apps.clone()], &resolver)?;
    store.lock().execute_batch("CREATE TRIGGER reject_native_grouping BEFORE INSERT ON app_variants BEGIN SELECT RAISE(ABORT, 'native grouping commit failure'); END")?;
    ensure!(
        super::super::desktop_import::regroup_and_persist(&registry, &store).is_err()
            && registry.logical_index().rows() == committed,
        "SQL-aborted grouping was visible before commit"
    );
    store
        .lock()
        .execute_batch("DROP TRIGGER reject_native_grouping")?;
    super::super::desktop_import::regroup_and_persist(&registry, &store)?;
    std::fs::remove_file(&new_desktop)?;
    super::super::desktop_import::import(&registry, &store, &[apps.clone()], &resolver)?;
    super::super::desktop_import::regroup_and_persist(&registry, &store)?;
    announce_applications_changed(&server).await;
    tokio::time::timeout(Duration::from_secs(2), changes.next())
        .await?
        .context("uninstall catalogue signal")?;
    ensure!(
        !std::fs::read_to_string(&export)?.contains("application/pdf="),
        "absent explicit default remained in compatibility export"
    );
    ensure!(
        templar_associationd::store::load_db(&store)?
            .mime_defaults
            .get("application/pdf")
            == Some(&old),
        "catalogue change erased durable explicit selection"
    );
    drop(changes);
    drop(association);
    drop(proxy);
    drop(client);
    drop(server);
    daemon.kill().await?;
    daemon.wait().await?;
    println!(
        "PASS native actual catalogue/association consumer: startup/alias/uninstall export before signal, owned ResolveFile parity, SQL-aborted grouping hidden, durable stale selection retained"
    );
    Ok(())
}
