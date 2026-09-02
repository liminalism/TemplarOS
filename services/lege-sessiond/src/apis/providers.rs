//! `org.legeos.Providers1`.

use crate::apis::failed;
use async_trait::async_trait;
use lege_appd::Registry;
use lege_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    providers::{self, ProviderRegistry},
};
use lege_state::Store;
use lege_types::{AppId, ProviderCapability, ProviderRole, TrustTier};
use parking_lot::RwLock;
use std::sync::Arc;
use zbus::{fdo, object_server::SignalEmitter};

/// Resolving "who is calling" to the one installed copy that is calling.
///
/// A seam, so the gate below is testable without a bus and without a `/proc`. The real
/// implementation takes the same path `ResolveWindow` does: the peer's PID, its cgroup, the
/// registered launch unit that owns it.
#[async_trait]
pub trait CallerVariant: Send + Sync + std::fmt::Debug {
    /// The variant behind `sender`, or `None` when it cannot be established. `None` is a refusal,
    /// never a pass: an unidentifiable caller is exactly what an impostor looks like.
    async fn variant_of(&self, sender: &str) -> Option<AppId>;
}

/// The real one: bus name -> PID -> cgroup -> registered instance.
#[derive(Debug)]
pub struct PeerCgroupVariant {
    connection: zbus::Connection,
    registry: Arc<Registry>,
}

impl PeerCgroupVariant {
    pub fn new(connection: zbus::Connection, registry: Arc<Registry>) -> Self {
        Self {
            connection,
            registry,
        }
    }
}

#[async_trait]
impl CallerVariant for PeerCgroupVariant {
    async fn variant_of(&self, sender: &str) -> Option<AppId> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection).await.ok()?;
        let bus_name = zbus::names::BusName::try_from(sender).ok()?;
        let pid = dbus.get_connection_unix_process_id(bus_name).await.ok()?;
        let resolution = self
            .registry
            .resolve_window(pid, &lege_types::WindowClaims::default())
            .ok()?;
        // Only a launch the registry itself brokered counts. A claim-only match is precisely the
        // evidence an impostor can manufacture.
        resolution.confidence.is_verified().then_some(())?;
        resolution.app_id
    }
}

#[derive(Debug)]
pub struct ProvidersApi {
    registry: Arc<RwLock<ProviderRegistry>>,
    store: Store,
    apps: Arc<Registry>,
    caller: Arc<dyn CallerVariant>,
    history: Arc<SqliteHistory>,
}

impl ProvidersApi {
    pub fn new(
        registry: Arc<RwLock<ProviderRegistry>>,
        store: Store,
        apps: Arc<Registry>,
        caller: Arc<dyn CallerVariant>,
        history: Arc<SqliteHistory>,
    ) -> Self {
        Self {
            registry,
            store,
            apps,
            caller,
            history,
        }
    }

    /// The two gates on `Declare`, as one pure decision so they can be tested without a bus.
    ///
    /// `caller` is the variant the peer resolved to, or `None` when it could not be established —
    /// which is a refusal, not a pass: an unidentifiable caller is exactly what an impostor looks
    /// like. Returns the sentence a person sees, and the sentence the caller gets, on refusal.
    fn check_declare(
        &self,
        caller: Option<&AppId>,
        app_id: &AppId,
        role: &str,
    ) -> Result<(), Refusal> {
        let Some(caller) = caller else {
            return Err(Refusal {
                subject: Some(app_id.clone()),
                history: format!(
                    "A declaration of the {role} role for {app_id} was refused: the caller could \
                     not be identified as an installed application."
                ),
                caller_message:
                    "an application may only declare a role for itself, and this caller could not \
                     be identified. Launch it through LegeOS and try again."
                        .into(),
            });
        };
        if self.apps.resolve_variant(caller) != self.apps.resolve_variant(app_id) {
            return Err(Refusal {
                subject: Some(caller.clone()),
                history: format!(
                    "{caller} tried to declare the {role} role for {app_id}, which is a different \
                     application. The declaration was refused."
                ),
                caller_message: format!(
                    "{caller} may only declare a role for itself, not for {app_id}."
                ),
            });
        }
        let trust = self
            .apps
            .manifest_resolved(app_id)
            .map_or(TrustTier::User, |manifest| manifest.trust());
        if !trust.may_hold_exclusive_role() {
            return Err(Refusal {
                subject: Some(app_id.clone()),
                history: format!(
                    "{app_id} asked to fill the {role} role. It was refused: the application comes \
                     from a file this account can rewrite, and an exclusive role has to come from \
                     something the system installed."
                ),
                caller_message: format!(
                    "{app_id} comes from a file this account can rewrite, so it cannot take the \
                     exclusive {role} role. Install it as a package or a Flatpak and it can."
                ),
            });
        }
        Ok(())
    }

    /// Record a refused declaration. A person looking at History1 should be able to see what asked
    /// for a role and why it was turned down, rather than wonder why nothing happened.
    fn record_refusal(&self, app_id: Option<AppId>, message: String) {
        let event = Event {
            id: EventId::new(),
            at_ms: lege_state::now_ms(),
            kind: EventKind::ProviderChange,
            app_id,
            instance_id: None,
            message,
            cause: Some("provider-declaration-refused".into()),
            actions: Vec::new(),
            reversible: false,
        };
        if let Err(error) = self.history.append(event) {
            tracing::warn!(%error, "failed to record a refused provider declaration");
        }
    }
}

/// Why a declaration was turned down: what the person is told, and what the caller is told.
#[derive(Clone, Debug)]
struct Refusal {
    subject: Option<AppId>,
    history: String,
    caller_message: String,
}

fn role_by_name(name: &str) -> fdo::Result<ProviderRole> {
    ProviderRole::ALL
        .iter()
        .copied()
        .find(|role| role.as_str() == name)
        .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown provider role {name:?}")))
}

fn parse_app_id(app_id: &str) -> fdo::Result<AppId> {
    app_id
        .parse()
        .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))
}

#[zbus::interface(name = "org.legeos.Providers1")]
impl ProvidersApi {
    /// `(role, resolution-json)`
    async fn report(&self) -> Vec<(String, String)> {
        self.registry
            .read()
            .report()
            .into_iter()
            .map(|(role, resolution)| {
                (
                    role.to_string(),
                    serde_json::to_string(&resolution).unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Every application that declares `role`, best first: `(app_id, priority)`.
    ///
    /// `Report` alone cannot answer this. Its `DefaultedByPriority` resolution carries the
    /// losers, but a role that *has* an explicit selection resolves to `Selected`, which carries
    /// only the winner — so a client offering a person a choice would be able to show the
    /// alternatives right up until they picked one, and never again.
    async fn candidates(&self, role: &str) -> fdo::Result<Vec<(String, u32)>> {
        let role = role_by_name(role)?;
        Ok(self
            .registry
            .read()
            .candidates(role)
            .into_iter()
            .map(|(app_id, priority)| (app_id.to_string(), priority))
            .collect())
    }

    async fn select(
        &self,
        role: &str,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let role_enum = role_by_name(role)?;
        let app_id = parse_app_id(app_id)?;
        self.store
            .transaction(|tx| {
                providers::apply_selection_within(tx, role_enum, Some(&app_id), "providers1-select")
            })
            .map_err(failed)?;
        self.registry.write().select(role_enum, app_id);
        let _ = emitter.changed(role.to_owned()).await;
        Ok(())
    }

    /// Drop the explicit selection and let priority decide again.
    async fn deselect(
        &self,
        role: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let role_enum = role_by_name(role)?;
        self.store
            .transaction(|tx| {
                providers::apply_selection_within(tx, role_enum, None, "providers1-deselect")
            })
            .map_err(failed)?;
        self.registry.write().deselect(role_enum);
        let _ = emitter.changed(role.to_owned()).await;
        Ok(())
    }

    /// An application declares it can fill an exclusive role.
    ///
    /// Two gates, both closing holes that were open. First, a caller may only declare for *itself*:
    /// the peer's PID is resolved through its cgroup, the same path `ResolveWindow` takes, and a
    /// caller that names a different application — or that cannot be identified at all — is
    /// refused. Second, an exclusive role may only be taken by a variant the system owns: a
    /// `.desktop` file dropped into the person's own data directory must not be able to become the
    /// session's notification server or lock screen.
    async fn declare(
        &self,
        app_id: &str,
        role: &str,
        priority: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let role_enum = role_by_name(role)?;
        let app_id = parse_app_id(app_id)?;

        let sender = header.sender().map(ToString::to_string).unwrap_or_default();
        let caller = self.caller.variant_of(&sender).await;
        if let Err(refusal) = self.check_declare(caller.as_ref(), &app_id, role) {
            self.record_refusal(refusal.subject.clone(), refusal.history.clone());
            return Err(fdo::Error::AccessDenied(refusal.caller_message));
        }

        let capability = ProviderCapability {
            role: role_enum,
            priority,
        };
        self.store
            .transaction(|tx| providers::declare_within(tx, &app_id, &capability))
            .map_err(failed)?;
        self.registry.write().declare(app_id, capability);
        let _ = emitter.changed(role.to_owned()).await;
        Ok(())
    }

    async fn undeclare(
        &self,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        let affected_roles: Vec<ProviderRole> = {
            self.registry
                .read()
                .declarations
                .iter()
                .filter(|declaration| declaration.app_id == app_id)
                .map(|declaration| declaration.capability.role)
                .collect()
        };
        self.store
            .transaction(|tx| providers::undeclare_within(tx, &app_id))
            .map_err(failed)?;
        self.registry.write().undeclare_app(&app_id);
        for role in affected_roles {
            let _ = emitter.changed(role.to_string()).await;
        }
        Ok(())
    }

    /// A role's resolution may have changed; re-call `Report` for the current state.
    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>, role: String) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::{
        AppAliases, AppCatalog, AppManifest, AppOriginKind, AppProvenance, ResourceClass,
    };
    use std::{collections::BTreeSet, path::PathBuf};

    /// A caller identity scripted by the test rather than read off a bus.
    #[derive(Debug)]
    struct FixedCaller(Option<AppId>);

    #[async_trait]
    impl CallerVariant for FixedCaller {
        async fn variant_of(&self, _sender: &str) -> Option<AppId> {
            self.0.clone()
        }
    }

    fn manifest(id: &str, origin: AppOriginKind) -> AppManifest {
        AppManifest {
            id: id.parse().unwrap(),
            display_name: id.into(),
            primary_desktop_file_id: format!("{id}.desktop"),
            executable: PathBuf::from("/usr/bin/true"),
            aliases: AppAliases::default(),
            mime_types: BTreeSet::default(),
            resource_class: ResourceClass::Normal,
            supports_checkpoint: false,
            icon: None,
            categories: BTreeSet::default(),
            keywords: BTreeSet::default(),
            no_display: false,
            provenance: Some(AppProvenance::from_origin(origin)),
        }
    }

    /// A `ProvidersApi` over an in-memory store, with `caller` as whoever is calling.
    fn api(installed: &[AppManifest], caller: Option<&str>) -> (ProvidersApi, Store) {
        let store = Store::in_memory().unwrap();
        let apps = Arc::new(Registry::default());
        for manifest in installed {
            apps.insert_manifest(manifest.clone());
        }
        apps.regroup(&std::collections::BTreeMap::new());
        let api = ProvidersApi::new(
            Arc::new(RwLock::new(ProviderRegistry::default())),
            store.clone(),
            apps,
            Arc::new(FixedCaller(caller.map(|id| id.parse().unwrap()))),
            Arc::new(SqliteHistory::new(store.clone())),
        );
        (api, store)
    }

    fn declarations(store: &Store) -> i64 {
        store
            .lock()
            .query_row("SELECT COUNT(*) FROM provider_declarations", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn refusals(store: &Store) -> Vec<String> {
        let connection = store.lock();
        let mut statement = connection
            .prepare("SELECT message FROM history WHERE cause = 'provider-declaration-refused'")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn a_user_owned_desktop_file_cannot_take_an_exclusive_role() {
        // The hole: anyone who can write ~/.local/share/applications could declare itself the
        // session's notification server, and the highest priority wins.
        let (api, store) = api(
            &[manifest(
                "org.example.Impostor",
                AppOriginKind::UserDesktopEntry,
            )],
            Some("org.example.Impostor"),
        );
        let trust = api
            .apps
            .manifest(&"org.example.Impostor".parse().unwrap())
            .unwrap()
            .trust();
        assert_eq!(trust, TrustTier::User);
        assert!(!trust.may_hold_exclusive_role());

        let impostor: AppId = "org.example.Impostor".parse().unwrap();
        let refusal = api
            .check_declare(Some(&impostor), &impostor, "notification-server")
            .expect_err("a user-owned copy must not take an exclusive role");
        assert!(
            refusal.caller_message.contains("rewrite")
                && refusal.caller_message.contains("Install it as a package"),
            "the refusal has to say what to do about it: {refusal:?}"
        );
        api.record_refusal(refusal.subject.clone(), refusal.history.clone());
        assert_eq!(declarations(&store), 0, "nothing was granted");
        let recorded = refusals(&store);
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].contains("refused") && recorded[0].contains("rewrite"),
            "the history event has to say what happened and why: {recorded:?}"
        );
    }

    #[test]
    fn a_packaged_application_may_take_a_role() {
        let shell: AppId = "org.legeos.Shell".parse().unwrap();
        let (api, _store) = api(
            &[manifest("org.legeos.Shell", AppOriginKind::DistroPackage)],
            Some("org.legeos.Shell"),
        );
        assert!(
            api.check_declare(Some(&shell), &shell, "notification-server")
                .is_ok()
        );
    }

    #[test]
    fn an_application_may_not_declare_a_role_for_a_different_one() {
        // The other half of the hole: Declare took an app_id from the caller and believed it.
        let shell: AppId = "org.legeos.Shell".parse().unwrap();
        let other: AppId = "org.example.Other".parse().unwrap();
        let (api, _store) = api(
            &[
                manifest("org.legeos.Shell", AppOriginKind::DistroPackage),
                manifest("org.example.Other", AppOriginKind::DistroPackage),
            ],
            Some("org.example.Other"),
        );
        let refusal = api
            .check_declare(Some(&other), &shell, "notification-server")
            .expect_err("declaring on another application's behalf must be refused");
        assert!(
            refusal
                .caller_message
                .contains("only declare a role for itself")
        );
    }

    #[test]
    fn a_caller_that_cannot_be_identified_is_refused_rather_than_trusted() {
        let shell: AppId = "org.legeos.Shell".parse().unwrap();
        let (api, _store) = api(
            &[manifest("org.legeos.Shell", AppOriginKind::DistroPackage)],
            None,
        );
        let refusal = api
            .check_declare(None, &shell, "notification-server")
            .expect_err("an unidentifiable caller is what an impostor looks like");
        assert!(refusal.caller_message.contains("could not be identified"));
    }

    #[test]
    fn a_manifest_with_no_provenance_is_not_treated_as_the_systems() {
        let anonymous_id: AppId = "org.example.Anonymous".parse().unwrap();
        let mut anonymous = manifest("org.example.Anonymous", AppOriginKind::DistroPackage);
        anonymous.provenance = None;
        let (api, _store) = api(&[anonymous], Some("org.example.Anonymous"));
        assert!(
            api.check_declare(Some(&anonymous_id), &anonymous_id, "lock-screen")
                .is_err(),
            "an application that says nothing about where it came from is not the system's"
        );
    }
}
