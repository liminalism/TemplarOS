use std::{
    collections::HashSet,
    io::{BufRead as _, BufReader},
    os::unix::{
        ffi::{OsStrExt as _, OsStringExt as _},
        fs::PermissionsExt as _,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use templar_client::{
    file_index::{self, IndexRoots, IndexSearch, Observation, SearchSession},
    proxies::{FilesBrowseProxy, FilesIndexProxy, FilesOperationsProxy, FilesTrashProxy},
};
use templar_file_index::{Query, RootPolicy};

struct OwnedProcess(Option<Child>);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_thousand_paged_matches_refresh_consistently_after_renames() {
    use templar_client::file_index::IndexCallError;
    let mut fixture = PrivateRuntime::new();
    for n in 0..2000 {
        std::fs::write(fixture.home.join(format!("paging-{n:04}.txt")), b"fixture").unwrap();
    }
    fixture.start(None);
    let connection = zbus::connection::Builder::address(fixture.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    await_roots(&connection, |roots| !roots.roots.is_empty()).await;
    let observation = Observation::acquire(&connection).await.unwrap();
    await_roots(&connection, |roots| {
        roots
            .roots
            .iter()
            .any(|root| root.current() && root.entry_count == 2001)
    })
    .await;
    let query = Query {
        text: "paging-".into(),
        page_size: 100,
        client_tag: Some("private-paged-view".into()),
        ..Query::default()
    };
    let session = SearchSession::search(&connection, &query).await.unwrap();
    assert_eq!(session.retained(), 2000);
    assert_eq!(session.result().total_files, Some(2000));
    assert_eq!(session.result().total_folders, Some(0));
    for n in 0..10 {
        std::fs::rename(
            fixture.home.join(format!("paging-{n:04}.txt")),
            fixture.home.join(format!("paging-renamed-{n:04}.txt")),
        )
        .unwrap();
    }
    await_search(&connection, "paging-renamed-", |result| {
        result.total == Some(10)
    })
    .await;
    let mut invalidated = false;
    for page in 0..20 {
        match session.fetch_page(page * 100, 100).await {
            Ok(rows) => {
                assert_eq!(rows.generation, session.result().generation);
                assert!(
                    rows.hits
                        .iter()
                        .all(|hit| !hit.present || hit.row.written_generation <= rows.generation)
                );
                invalidated |= rows.dropped > 0;
            }
            Err(IndexCallError::SnapshotRequired(_)) => invalidated = true,
            Err(error) => panic!("unexpected page error: {error}"),
        }
    }
    assert!(
        invalidated,
        "renamed rows must invalidate or drop their old cursor positions"
    );
    session.close().await.unwrap();
    let fresh = SearchSession::search(&connection, &query).await.unwrap();
    let mut paths = Vec::new();
    for page in 0..20 {
        let rows = fresh.fetch_page(page * 100, 100).await.unwrap();
        assert_eq!(rows.dropped, 0);
        paths.extend(
            rows.hits
                .into_iter()
                .map(|hit| hit.row.path.native_path().unwrap()),
        );
    }
    assert_eq!(paths.len(), 2000);
    let actual: HashSet<_> = paths.iter().cloned().collect();
    let expected: HashSet<_> = std::fs::read_dir(&fixture.home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(actual, expected);
    // A second query independently reproduces the complete ordering, not just the set.
    let oracle = SearchSession::search(&connection, &query).await.unwrap();
    let mut oracle_paths = Vec::new();
    for page in 0..20 {
        oracle_paths.extend(
            oracle
                .fetch_page(page * 100, 100)
                .await
                .unwrap()
                .hits
                .into_iter()
                .map(|hit| hit.row.path.native_path().unwrap()),
        );
    }
    assert_eq!(paths, oracle_paths);
    fresh.close().await.unwrap();
    oracle.close().await.unwrap();
    observation.close().await.unwrap();
}
impl OwnedProcess {
    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if child.try_wait().unwrap().is_none() {
                child.kill().unwrap();
            }
            child.wait().unwrap();
        }
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

struct PrivateRuntime {
    directory: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    index: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    address: String,
    _bus: OwnedProcess,
    filed: OwnedProcess,
}
impl PrivateRuntime {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let home = root.join("home");
        let config = root.join("config");
        let data = root.join("data");
        let runtime = root.join("runtime");
        for path in [&home, &config, &data, &runtime] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:tmpdir={}", runtime.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("private dbus-daemon prerequisite");
        let mut bus = OwnedProcess(Some(bus));
        let mut address = String::new();
        BufReader::new(bus.0.as_mut().unwrap().stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        assert!(
            address.starts_with("unix:"),
            "private bus did not return a Unix address"
        );
        Self {
            index: root.join("index.sqlite3"),
            state: root.join("session.db"),
            home,
            config,
            data,
            runtime,
            address: address.trim().to_owned(),
            directory,
            _bus: bus,
            filed: OwnedProcess(None),
        }
    }
    fn start(&mut self, budget: Option<u64>) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_templar-filed"));
        command.args(["--state-db", self.state.to_str().unwrap(), "--index-db", self.index.to_str().unwrap()])
            .env("HOME", &self.home).env("XDG_CONFIG_HOME", &self.config).env("XDG_DATA_HOME", &self.data)
            .env("XDG_STATE_HOME", self.directory.path()).env("XDG_CACHE_HOME", self.directory.path())
            .env("XDG_RUNTIME_DIR", &self.runtime).env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            // Even optional host/udisks calls are isolated; never contact the owner's live buses.
            .env("DBUS_SYSTEM_BUS_ADDRESS", &self.address).env("TEMPLAR_INDEX_FIXTURE_GUARD", "1")
            .env("RUST_LOG", "warn").stdout(Stdio::null()).stderr(Stdio::inherit());
        if let Some(budget) = budget {
            command.args(["--index-max-entries", &budget.to_string()]);
        }
        self.filed = OwnedProcess(Some(command.spawn().unwrap()));
    }
}

async fn await_roots(
    connection: &zbus::Connection,
    predicate: impl Fn(&IndexRoots) -> bool,
) -> IndexRoots {
    let deadline = Instant::now() + Duration::from_secs(15);
    let proxy = FilesIndexProxy::new(connection).await.unwrap();
    loop {
        if let Ok(Ok(roots)) =
            tokio::time::timeout(Duration::from_secs(2), file_index::roots(&proxy)).await
            && predicate(&roots)
        {
            return roots;
        }
        assert!(
            Instant::now() < deadline,
            "private index root condition did not converge"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn await_search(
    connection: &zbus::Connection,
    query: &str,
    predicate: impl Fn(&IndexSearch) -> bool,
) -> IndexSearch {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let session = SearchSession::search(connection, &Query::new(query).with_page_size(100))
            .await
            .unwrap();
        let result = session.result().clone();
        session.close().await.unwrap();
        if predicate(&result) {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "private indexed mutation did not converge: {query}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
fn uri_path(path: &Path) -> String {
    templar_filed::actions::file_uri(path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_index_warm_pages_external_mutations_restart_native_actions_and_bookmark_preservation()
 {
    let mut fixture = PrivateRuntime::new();
    std::fs::create_dir(fixture.home.join("nested")).unwrap();
    let original = fixture.home.join("warm ; $(not-a-command) é.txt");
    std::fs::write(&original, b"old").unwrap();
    let native = fixture
        .home
        .join(std::ffi::OsString::from_vec(b"warm-native-\xff".to_vec()));
    std::fs::write(&native, b"native").unwrap();
    for number in 0..256 {
        std::fs::write(fixture.home.join(format!("warm-{number:03}.dat")), b"row").unwrap();
    }
    let bookmark = fixture.home.join("place ; é $(not-a-command)");
    std::fs::create_dir(&bookmark).unwrap();
    let gtk = fixture.config.join("gtk-3.0/bookmarks");
    std::fs::create_dir_all(gtk.parent().unwrap()).unwrap();
    let unmanaged = b"smb://host/share Native\nfile:///unmanaged Other\n#opaque\xff";
    std::fs::write(&gtk, unmanaged).unwrap();
    fixture.start(None);
    let connection = zbus::connection::Builder::address(fixture.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let initial = await_roots(&connection, |roots| {
        !roots.roots.is_empty()
            && roots
                .roots
                .iter()
                .all(|root| root.current() && root.metadata == "verified")
    })
    .await;
    assert!(initial.roots[0].entry_count >= 260);
    let snapshot_proxy = FilesIndexProxy::new(&connection).await.unwrap();
    let _snapshot = assert_sealed_snapshot(&snapshot_proxy).await;
    let visible = assert_warm_pages_and_native_actions(&connection, &native).await;

    assert_external_mutations(&fixture, &connection, &original, &visible).await;

    let native_edit = assert_bookmark_preservation(&connection, &bookmark, &gtk, unmanaged).await;

    assert_restart_and_native_root_persistence(
        &mut fixture,
        &connection,
        visible,
        &initial,
        &gtk,
        &native_edit,
    )
    .await;
    // Only owned PIDs are stopped by RAII. No user units, live bus, loginctl or deployment is used.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bulk_and_metadata_search_repair_namespace_only_truth_on_private_bus() {
    let mut fixture = PrivateRuntime::new();
    let payload = fixture.home.join("payload.txt");
    std::fs::write(&payload, b"measured").unwrap();
    fixture.start(None);
    let connection = zbus::connection::Builder::address(fixture.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let proxy = FilesIndexProxy::new(&connection).await.unwrap();
    await_roots(&connection, |roots| roots.roots.len() == 1).await;
    let observation = Observation::acquire(&connection).await.unwrap();
    let initial = await_roots(&connection, |roots| {
        roots.roots.len() == 1 && roots.roots[0].current() && roots.roots[0].metadata == "verified"
    })
    .await;
    let id = initial.roots[0].root;
    let mut policy = file_index::root_policy(&proxy, &fixture.home, &initial.roots[0].policy_hash)
        .await
        .unwrap();
    policy.include_hidden = !policy.include_hidden;
    file_index::configure_root(
        &proxy,
        &fixture.home,
        &policy,
        &initial.roots[0].policy_hash,
    )
    .await
    .unwrap();
    await_roots(&connection, |roots| {
        roots.roots[0].generation > initial.roots[0].generation
            && roots.roots[0].current()
            && roots.roots[0].metadata == "unverified"
    })
    .await;
    let partial = templar_client::tree_source::load_snapshot_tree(&connection, &fixture.home)
        .await
        .unwrap();
    assert!(
        !partial.complete
            && partial
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("metadata unverified"))
    );
    let measured = await_roots(&connection, |roots| {
        roots.roots[0].current() && roots.roots[0].metadata == "verified"
    })
    .await;
    let repaired = templar_client::tree_source::load_snapshot_tree(&connection, &fixture.home)
        .await
        .unwrap();
    assert!(repaired.complete);
    assert!(
        repaired
            .entries
            .iter()
            .any(|entry| entry.path == payload && entry.size == 8)
    );

    // The filename consumer can deliberately request metadata without opening a bulk frame.
    policy.include_hidden = !policy.include_hidden;
    file_index::configure_root(
        &proxy,
        &fixture.home,
        &policy,
        &measured.roots[0].policy_hash,
    )
    .await
    .unwrap();
    await_roots(&connection, |roots| {
        roots.roots[0].generation > measured.roots[0].generation
            && roots.roots[0].current()
            && roots.roots[0].metadata == "unverified"
    })
    .await;
    let query = Query {
        text: "payload".into(),
        require_metadata: true,
        ..Query::default()
    };
    let (cursor, _, result) = proxy
        .search(&serde_json::to_string(&query).unwrap())
        .await
        .unwrap();
    let result: IndexSearch = serde_json::from_str(&result).unwrap();
    assert!(!result.current());
    assert_eq!(result.stale_metadata_roots, vec![id]);
    proxy.close_cursor(&cursor).await.unwrap();
    let verified = await_roots(&connection, |roots| {
        roots.roots[0].current() && roots.roots[0].metadata == "verified"
    })
    .await;
    assert_eq!(verified.roots[0].native_path().unwrap(), fixture.home);
    observation.close().await.unwrap();
}

/// A cache the daemon cannot open costs the index and nothing else: browsing answers, `Roots`
/// reports why the index is unavailable, an index call names `Error.Unavailable`, and once the
/// cause is fixed `Reconcile` reopens it and the home root comes back current.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_unavailable_keeps_file_operations_serving() {
    let mut fixture = PrivateRuntime::new();
    std::fs::write(fixture.home.join("served-anyway.txt"), b"still here").unwrap();
    let cache = fixture.directory.path().join("unwritable-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o500)).unwrap();
    fixture.index = cache.join("index.sqlite3");
    fixture.start(None);
    let connection = zbus::connection::Builder::address(fixture.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();

    let unavailable = await_roots(&connection, |roots| {
        !roots.diagnostics.availability.is_available()
    })
    .await;
    assert!(unavailable.roots.is_empty());
    assert!(
        unavailable
            .diagnostics
            .availability
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("permission")),
        "{:?}",
        unavailable.diagnostics.availability
    );
    let browse = FilesBrowseProxy::new(&connection).await.unwrap();
    let started = Instant::now();
    let inspected = browse
        .inspect(fixture.home.join("served-anyway.txt").to_str().unwrap())
        .await
        .unwrap();
    assert!(inspected.contains("served-anyway.txt"), "{inspected}");
    assert!(started.elapsed() < Duration::from_secs(2));
    let proxy = FilesIndexProxy::new(&connection).await.unwrap();
    let refused = proxy
        .search(&serde_json::to_string(&Query::new("served")).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.templaros.Files.Index1.Error.Unavailable"),
        "{refused:?}"
    );

    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700)).unwrap();
    proxy.reconcile("{}").await.unwrap();
    let observation = Observation::acquire(&connection).await.unwrap();
    let restored = await_roots(&connection, |roots| {
        roots.diagnostics.availability.is_available()
            && roots.roots.len() == 1
            && roots.roots[0].current()
    })
    .await;
    assert_eq!(restored.roots[0].native_path().unwrap(), fixture.home);
    await_search(&connection, "served-anyway", |result| {
        result.hits.len() == 1
    })
    .await;
    drop(observation);
}

/// A client that disappears without closing its cursor stops pinning it the moment its bus name
/// goes, not at the cursor's idle expiry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_departed_callers_cursor_is_released() {
    let mut fixture = PrivateRuntime::new();
    std::fs::write(fixture.home.join("pinned-row.txt"), b"x").unwrap();
    fixture.start(None);
    let connection = zbus::connection::Builder::address(fixture.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    await_roots(&connection, |roots| roots.roots.len() == 1).await;
    let observation = Observation::acquire(&connection).await.unwrap();
    await_roots(&connection, |roots| {
        roots.roots.len() == 1 && roots.roots[0].current()
    })
    .await;

    let leaving = zbus::connection::Builder::address(fixture.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    let proxy = FilesIndexProxy::new(&leaving).await.unwrap();
    let (cursor, _, _) = proxy
        .search(&serde_json::to_string(&Query::new("pinned")).unwrap())
        .await
        .unwrap();
    assert!(!cursor.is_empty());
    await_roots(&connection, |roots| roots.diagnostics.cursor_count == 1).await;
    // A proxy holds a connection clone. graceful_shutdown waits for every clone, so retaining
    // the proxy makes the departure fixture wait forever before the bus name can disappear.
    drop(proxy);
    tokio::time::timeout(Duration::from_secs(5), leaving.graceful_shutdown())
        .await
        .expect("the owned client closes without retained proxies");
    await_roots(&connection, |roots| roots.diagnostics.cursor_count == 0).await;
    drop(observation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eight_typing_clients_leave_browse_responsive() {
    let mut fixture = PrivateRuntime::new();
    let browse_dir = fixture.home.join("browse");
    std::fs::create_dir(&browse_dir).unwrap();
    std::fs::write(browse_dir.join("visible.txt"), b"visible").unwrap();
    for number in 0..5_000 {
        std::fs::write(fixture.home.join(format!("row-{number:05}.txt")), b"row").unwrap();
    }
    fixture.start(None);
    let connection = zbus::connection::Builder::address(fixture.address.as_str())
        .unwrap()
        .build()
        .await
        .unwrap();
    await_roots(&connection, |roots| roots.roots.len() == 1).await;
    let observation = Observation::acquire(&connection).await.unwrap();
    await_roots(&connection, |roots| {
        roots.roots.len() == 1 && roots.roots[0].current()
    })
    .await;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut clients = Vec::new();
    for client in 0..8 {
        let address = fixture.address.clone();
        let done = done.clone();
        clients.push(tokio::spawn(async move {
            let connection = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let proxy = FilesIndexProxy::new(&connection).await.unwrap();
            for round in 0..20 {
                let mut query = Query::new(["r", "ro", "row", "row-0"][(round + client) % 4]);
                query.client_tag = Some("typing".to_owned());
                match proxy.search(&serde_json::to_string(&query).unwrap()).await {
                    Ok((cursor, _, _)) => proxy.close_cursor(&cursor).await.unwrap(),
                    Err(zbus::Error::MethodError(name, _, _))
                        if matches!(
                            name.as_str(),
                            "org.templaros.Files.Index1.Error.Busy"
                                | "org.templaros.Files.Index1.Error.Cancelled"
                        ) => {}
                    Err(error) => panic!("typing client failed: {error}"),
                }
            }
            done.fetch_add(1, std::sync::atomic::Ordering::Release);
        }));
    }
    let browse = FilesBrowseProxy::new(&connection).await.unwrap();
    let mut samples = 0;
    let mut worst = Duration::ZERO;
    while done.load(std::sync::atomic::Ordering::Acquire) < 8 {
        let started = Instant::now();
        let listing = tokio::time::timeout(
            Duration::from_millis(100),
            browse.list_directory(browse_dir.to_str().unwrap(), false, 10),
        )
        .await
        .expect("Browse1 exceeded 100 ms during eight concurrent typing clients")
        .unwrap();
        worst = worst.max(started.elapsed());
        assert!(listing.contains("visible.txt"));
        samples += 1;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    for client in clients {
        client.await.unwrap();
    }
    assert!(samples > 0);
    println!("160 typing queries: Browse1 {samples} samples, worst {worst:?}");
    await_roots(&connection, |roots| roots.diagnostics.cursor_count == 0).await;
    drop(observation);
}

async fn assert_sealed_snapshot(snapshot_proxy: &FilesIndexProxy<'_>) -> std::fs::File {
    let (descriptor, manifest_json) = snapshot_proxy.open_snapshot("{}").await.unwrap();
    let manifest: templar_file_index::snapshot::SnapshotManifest =
        serde_json::from_str(&manifest_json).unwrap();
    assert!(manifest.sealed);
    let mut snapshot = std::fs::File::from(std::os::fd::OwnedFd::from(descriptor));
    let seals = rustix::fs::fcntl_get_seals(&snapshot).unwrap();
    let required = rustix::fs::SealFlags::WRITE
        | rustix::fs::SealFlags::GROW
        | rustix::fs::SealFlags::SHRINK
        | rustix::fs::SealFlags::SEAL;
    assert!(
        seals.contains(required),
        "snapshot publication was not kernel sealed"
    );
    let access = rustix::fs::fcntl_getfl(&snapshot).unwrap();
    assert_eq!(
        access & rustix::fs::OFlags::ACCMODE,
        rustix::fs::OFlags::RDONLY,
        "snapshot descriptor grants writable access"
    );
    assert_eq!(
        std::io::Write::write_all(&mut snapshot, b"tamper")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EBADF)
    );
    assert!(snapshot.set_len(0).is_err());
    assert!(
        snapshot
            .set_len(snapshot.metadata().unwrap().len() + 1)
            .is_err()
    );
    snapshot
}

async fn assert_warm_pages_and_native_actions(
    connection: &zbus::Connection,
    native: &Path,
) -> SearchSession {
    let gui_thread_session = SearchSession::search(connection, &Query::new("warm"))
        .await
        .unwrap();
    std::thread::spawn(move || drop(gui_thread_session))
        .join()
        .unwrap();
    await_roots(connection, |roots| roots.diagnostics.cursor_count == 0).await;
    let visible = SearchSession::search(connection, &Query::new("warm").with_page_size(100))
        .await
        .unwrap();
    assert_eq!(visible.result().total, Some(258));
    assert!(!visible.result().truncated);
    let mut ids: HashSet<i64> = visible
        .result()
        .hits
        .iter()
        .map(|hit| hit.row.entry_id)
        .collect();
    for offset in [100, 200] {
        let page = visible.fetch_page(offset, 100).await.unwrap();
        for hit in page.hits {
            assert!(ids.insert(hit.row.entry_id), "paging repeated an entry");
        }
    }
    assert_eq!(
        ids.len(),
        258,
        "paging lost entries beyond the first visible page"
    );
    let native_result =
        await_search(connection, "warm-native", |result| result.hits.len() == 1).await;
    assert_eq!(
        native_result.hits[0]
            .row
            .path
            .native_path()
            .unwrap()
            .as_os_str()
            .as_bytes(),
        native.as_os_str().as_bytes()
    );
    let uri = file_index::render_native_path(connection, native, "uri")
        .await
        .unwrap();
    assert_eq!(templar_filed::actions::path_from_uri(&uri).unwrap(), native);
    let shell = file_index::render_native_path(connection, native, "shell-quoted")
        .await
        .unwrap();
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            &format!("printf '%s' {shell}"),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, native.as_os_str().as_bytes());
    assert!(
        file_index::render_native_path(connection, native, "as-shown")
            .await
            .is_err()
    );
    visible
}

async fn assert_external_mutations(
    fixture: &PrivateRuntime,
    connection: &zbus::Connection,
    original: &Path,
    visible: &SearchSession,
) {
    // Filesystem operations, as performed by an external file manager: no filed undo is invented.
    std::fs::write(original, b"updated file contents").unwrap();
    await_search(connection, "not-a-command", |result| {
        result
            .hits
            .iter()
            .any(|hit| hit.row.path.native_path().unwrap() == original && hit.row.size_bytes == 21)
    })
    .await;
    assert!(
        visible.fetch_page(0, 1000).await.is_err(),
        "updated rows must not masquerade as the pinned older generation"
    );
    let renamed = fixture.home.join("renamed ; é.txt");
    std::fs::rename(original, &renamed).unwrap();
    await_search(connection, "renamed", |result| {
        result
            .hits
            .iter()
            .any(|hit| hit.row.path.native_path().unwrap() == renamed)
    })
    .await;
    let moved = fixture.home.join("nested/moved ; é.txt");
    std::fs::rename(&renamed, &moved).unwrap();
    let moved_result = await_search(connection, "moved", |result| {
        result
            .hits
            .iter()
            .any(|hit| hit.row.path.native_path().unwrap() == moved)
    })
    .await;
    assert!(
        moved_result.hits[0].row.parent.is_some(),
        "watcher move lost parent identity"
    );
    await_search(connection, "renamed", |result| result.hits.is_empty()).await;
    let trash = fixture.data.join("Trash");
    std::fs::create_dir_all(trash.join("files")).unwrap();
    std::fs::create_dir_all(trash.join("info")).unwrap();
    std::fs::write(
        trash.join("info/moved.trashinfo"),
        format!(
            "[Trash Info]\nPath={}\nDeletionDate=2026-10-05T00:00:00\n",
            uri_path(&moved).strip_prefix("file://").unwrap()
        ),
    )
    .unwrap();
    std::fs::rename(&moved, trash.join("files/moved")).unwrap();
    await_search(connection, "moved", |result| result.hits.is_empty()).await;
    let operations = FilesOperationsProxy::new(connection).await.unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&operations.recent(20).await.unwrap()).unwrap(),
        serde_json::json!([])
    );
    let trash_proxy = FilesTrashProxy::new(connection).await.unwrap();
    let items: serde_json::Value =
        serde_json::from_str(&trash_proxy.list().await.unwrap()).unwrap();
    assert!(
        items
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.to_string().contains("moved"))
    );
}

async fn assert_bookmark_preservation(
    connection: &zbus::Connection,
    bookmark: &Path,
    gtk: &Path,
    unmanaged: &[u8],
) -> Vec<u8> {
    let browse = FilesBrowseProxy::new(connection).await.unwrap();
    browse
        .set_bookmark(bookmark.to_str().unwrap(), true)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(gtk).unwrap(),
        unmanaged,
        "bookmark mutation exported implicitly"
    );
    let export: serde_json::Value =
        serde_json::from_str(&browse.export_bookmarks().await.unwrap()).unwrap();
    assert_eq!(export["added"], 1);
    assert_eq!(
        std::fs::read(export["backup"].as_str().unwrap()).unwrap(),
        unmanaged
    );
    let exported = std::fs::read(gtk).unwrap();
    assert!(exported.starts_with(unmanaged));
    let native_edit = [exported.as_slice(), b"file:///native-later PCManFM\n"].concat();
    std::fs::write(gtk, &native_edit).unwrap();
    browse
        .set_bookmark(bookmark.to_str().unwrap(), false)
        .await
        .unwrap();
    browse.export_bookmarks().await.unwrap();
    assert_eq!(std::fs::read(gtk).unwrap(), native_edit);
    native_edit
}

async fn assert_restart_and_native_root_persistence(
    fixture: &mut PrivateRuntime,
    connection: &zbus::Connection,
    visible: SearchSession,
    initial: &IndexRoots,
    gtk: &Path,
    native_edit: &[u8],
) {
    fixture.filed.stop();
    fixture.start(Some(1));
    let restarted = await_roots(connection, |roots| {
        roots.roots.iter().any(|root| {
            root.state == "stale"
                && !root.complete
                && root
                    .reason
                    .as_ref()
                    .is_some_and(|reason| reason.contains("budget"))
        })
    })
    .await;
    assert!(
        restarted.roots[0].entry_count >= 250,
        "bounded restart discarded the previous visible generation"
    );
    assert!(
        visible.fetch_page(0, 100).await.is_err(),
        "daemon restart silently preserved a foreign cursor"
    );
    let reacquired = Observation::acquire(connection).await.unwrap();
    visible.close().await.unwrap(); // Old observation release cannot consume the new token.
    let native_root = fixture
        .directory
        .path()
        .join(std::ffi::OsString::from_vec(b"root-\xff".to_vec()));
    std::fs::create_dir(&native_root).unwrap();
    std::fs::write(native_root.join("persisted.txt"), b"choice").unwrap();
    let policy = RootPolicy {
        monitoring: false,
        ..RootPolicy::default_home()
    };
    let proxy = FilesIndexProxy::new(connection).await.unwrap();
    file_index::configure_root(&proxy, &native_root, &policy, "absent")
        .await
        .unwrap();
    file_index::remove_root(&proxy, &fixture.home, &restarted.roots[0].policy_hash)
        .await
        .unwrap();
    assert!(
        proxy
            .remove_root(
                &file_index::encode_path(&fixture.home),
                &restarted.roots[0].policy_hash
            )
            .await
            .is_err(),
        "a repeated stale remove mutated a different native root"
    );
    await_roots(connection, |roots| {
        roots.roots.len() == 1 && roots.roots[0].native_path().unwrap() == native_root
    })
    .await;
    reacquired.close().await.unwrap();
    assert_native_root_survives_cache_replacement(
        fixture,
        connection,
        &proxy,
        &native_root,
        initial,
        gtk,
        native_edit,
    )
    .await;
}

async fn assert_native_root_survives_cache_replacement(
    fixture: &mut PrivateRuntime,
    connection: &zbus::Connection,
    proxy: &FilesIndexProxy<'_>,
    native_root: &Path,
    initial: &IndexRoots,
    gtk: &Path,
    native_edit: &[u8],
) {
    fixture.filed.stop();
    for path in [
        fixture.index.clone(),
        fixture.index.with_file_name("index.sqlite3-wal"),
        fixture.index.with_file_name("index.sqlite3-shm"),
    ] {
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    fixture.start(None);
    let restored = await_roots(connection, |roots| {
        roots.roots.len() == 1
            && roots.roots[0].native_path().unwrap() == native_root
            && roots.roots[0].complete
            && roots.roots[0].reason.as_deref() == Some("unwatched")
    })
    .await;
    assert!(
        !file_index::root_policy(proxy, native_root, &restored.roots[0].policy_hash)
            .await
            .unwrap()
            .monitoring
    );
    assert_ne!(restored.cache_identity, initial.cache_identity);
    let stale_rebuild = serde_json::json!({"cache_identity":initial.cache_identity,"roots":[initial.roots[0].root]});
    assert!(
        proxy.reconcile(&stale_rebuild.to_string()).await.is_err(),
        "stale rebuild crossed cache lineage"
    );
    assert_ne!(
        restored.roots[0].native_path().unwrap(),
        fixture.home,
        "cache replacement resurrected removed HOME"
    );
    assert_eq!(
        std::fs::read(gtk).unwrap(),
        native_edit,
        "relogin/export destroyed unmanaged bookmarks"
    );
    let persisted = await_search(connection, "persisted", |result| result.hits.len() == 1).await;
    assert_eq!(
        persisted.hits[0].row.path.native_path().unwrap(),
        native_root.join("persisted.txt")
    );
    assert!(
        !persisted.current(),
        "disabled monitoring falsely claimed current truth"
    );
}
