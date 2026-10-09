//! The declared gnome-keyring Secret Service is the durable authority; no plaintext fallback.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Type, Value};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "org.freedesktop.secrets";
const ROOT: &str = "/org/freedesktop/secrets";
const TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Serialize, Deserialize, Type)]
struct Secret {
    session: OwnedObjectPath,
    parameters: Vec<u8>,
    value: Vec<u8>,
    content_type: String,
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}
struct Session {
    path: OwnedObjectPath,
    authority: zbus::names::OwnedUniqueName,
    connection: zbus::Connection,
}
impl Drop for Session {
    fn drop(&mut self) {
        let path = self.path.clone();
        let connection = self.connection.clone();
        let authority = self.authority.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(TIMEOUT, async {
                let proxy = zbus::Proxy::new(
                    &connection,
                    authority.as_str(),
                    path.as_str(),
                    "org.freedesktop.Secret.Session",
                )
                .await?;
                proxy.call::<_, _, ()>("Close", &()).await
            })
            .await;
        });
    }
}
#[derive(Clone)]
pub struct SecretStore {
    connection: zbus::Connection,
}
impl SecretStore {
    pub fn new(connection: zbus::Connection) -> Self {
        Self { connection }
    }
    async fn proxy<'a>(&'a self, path: &'a str, interface: &'a str) -> Result<zbus::Proxy<'a>> {
        Ok(zbus::Proxy::new(&self.connection, SERVICE, path, interface).await?)
    }
    fn attributes(uuid: &str) -> HashMap<&str, &str> {
        HashMap::from([
            ("xdg:schema", "org.freedesktop.NetworkManager.Connection"),
            ("connection-uuid", uuid),
            ("setting-name", "802-11-wireless-security"),
            ("setting-key", "psk"),
        ])
    }
    async fn session(&self, authority: &zbus::names::OwnedUniqueName) -> Result<Session> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            authority.as_str(),
            ROOT,
            "org.freedesktop.Secret.Service",
        )
        .await?;
        let (output, session): (OwnedValue, OwnedObjectPath) = proxy.call("OpenSession", &("plain", Value::from(""))).await
            .context("gnome-keyring Secret Service unavailable; start/unlock the declared secret provider")?;
        ensure!(
            session.as_str() != "/" && <&str>::try_from(&output)?.is_empty(),
            "credential provider did not establish the requested Secret Service session"
        );
        Ok(Session {
            path: session,
            authority: authority.clone(),
            connection: self.connection.clone(),
        })
    }
    async fn collection(&self) -> Result<(OwnedObjectPath, zbus::names::OwnedUniqueName)> {
        let proxy = self.proxy(ROOT, "org.freedesktop.Secret.Service").await?;
        let reply = proxy.call_method("ReadAlias", &("default",)).await?;
        let authority: zbus::names::OwnedUniqueName = reply
            .header()
            .sender()
            .context("credential provider omitted its bus identity")?
            .to_owned()
            .into();
        let collection: OwnedObjectPath = reply.body().deserialize()?;
        ensure!(
            collection.as_str() != "/",
            "keyring has no default collection; create/unlock it with the existing credential provider"
        );
        let proxy = zbus::Proxy::new(
            &self.connection,
            authority.as_str(),
            collection.as_str(),
            "org.freedesktop.Secret.Collection",
        )
        .await?;
        ensure!(
            !proxy.get_property::<bool>("Locked").await?,
            "keyring is locked; unlock it with the existing credential provider or explicitly choose session-only credentials"
        );
        Ok((collection, authority))
    }
    pub async fn available(&self) -> Result<()> {
        tokio::time::timeout(TIMEOUT, self.collection())
            .await
            .context("credential store check timed out")??;
        Ok(())
    }
    pub async fn get(&self, uuid: &str) -> Result<Option<Zeroizing<String>>> {
        tokio::time::timeout(TIMEOUT, self.get_inner(uuid))
            .await
            .context("credential lookup timed out")?
    }
    async fn get_inner(&self, uuid: &str) -> Result<Option<Zeroizing<String>>> {
        let service = self.proxy(ROOT, "org.freedesktop.Secret.Service").await?;
        let reply = service
            .call_method("SearchItems", &(Self::attributes(uuid),))
            .await
            .context("credential store unavailable")?;
        let authority: zbus::names::OwnedUniqueName = reply
            .header()
            .sender()
            .context("credential provider omitted its bus identity")?
            .to_owned()
            .into();
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            reply.body().deserialize()?;
        ensure!(
            locked.is_empty(),
            "saved network credential is locked; unlock the keyring with the existing provider"
        );
        let Some(item) = unlocked.first() else {
            return Ok(None);
        };
        ensure!(
            unlocked.len() == 1,
            "credential store contains conflicting network credentials; review them with the existing provider"
        );
        let session = self.session(&authority).await?;
        let proxy = zbus::Proxy::new(
            &self.connection,
            authority.as_str(),
            item.as_str(),
            "org.freedesktop.Secret.Item",
        )
        .await?;
        let mut secret: Secret =
            proxy
                .call("GetSecret", &(&session.path,))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("credential store could not return the saved credential")
                })?;
        ensure!(
            secret.session == session.path
                && secret.parameters.is_empty()
                && secret.value.len() <= 4096
                && secret.content_type.starts_with("text/plain"),
            "saved network credential has unsupported format or session"
        );
        match String::from_utf8(std::mem::take(&mut secret.value)) {
            Ok(text) => Ok(Some(Zeroizing::new(text))),
            Err(error) => {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                anyhow::bail!("saved network credential is not UTF-8")
            }
        }
    }
    pub async fn put(&self, uuid: &str, label: &str, value: &str) -> Result<()> {
        tokio::time::timeout(TIMEOUT, self.put_inner(uuid, label, value))
            .await
            .context("credential save timed out")?
    }
    async fn put_inner(&self, uuid: &str, label: &str, value: &str) -> Result<()> {
        ensure!(
            !value.is_empty() && value.len() <= 4096,
            "credential length is outside the permitted bound"
        );
        let (collection, authority) = self.collection().await?;
        let session = self.session(&authority).await?;
        let properties: HashMap<&str, Value<'_>> = HashMap::from([
            ("org.freedesktop.Secret.Item.Label", Value::from(label)),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(Self::attributes(uuid)),
            ),
        ]);
        let secret = Secret {
            session: session.path.clone(),
            parameters: Vec::new(),
            value: value.as_bytes().to_vec(),
            content_type: "text/plain; charset=utf8".into(),
        };
        let proxy = zbus::Proxy::new(
            &self.connection,
            authority.as_str(),
            collection.as_str(),
            "org.freedesktop.Secret.Collection",
        )
        .await?;
        let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = proxy
            .call("CreateItem", &(properties, secret, true))
            .await
            .map_err(|_| {
                anyhow::anyhow!("keyring credential save failed; storage was not confirmed")
            })?;
        ensure!(
            prompt.as_str() == "/" && item.as_str() != "/",
            "keyring requires interactive confirmation; use the existing credential provider, storage was not confirmed"
        );
        Ok(())
    }
    pub async fn delete(&self, uuid: &str) -> Result<()> {
        tokio::time::timeout(TIMEOUT, self.delete_inner(uuid))
            .await
            .context("credential deletion timed out")?
    }
    async fn delete_inner(&self, uuid: &str) -> Result<()> {
        let service = self.proxy(ROOT, "org.freedesktop.Secret.Service").await?;
        let reply = service
            .call_method("SearchItems", &(Self::attributes(uuid),))
            .await?;
        let authority: zbus::names::OwnedUniqueName = reply
            .header()
            .sender()
            .context("credential provider omitted its bus identity")?
            .to_owned()
            .into();
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            reply.body().deserialize()?;
        ensure!(
            locked.is_empty(),
            "credential deletion requires unlocking the existing keyring"
        );
        ensure!(
            unlocked.len() <= 32,
            "credential store contains too many conflicting items"
        );
        for item in unlocked {
            let proxy = zbus::Proxy::new(
                &self.connection,
                authority.as_str(),
                item.as_str(),
                "org.freedesktop.Secret.Item",
            )
            .await?;
            let prompt: OwnedObjectPath = proxy.call("Delete", &()).await?;
            ensure!(
                prompt.as_str() == "/",
                "keyring requires confirmation before deleting this credential; use the existing provider"
            );
        }
        Ok(())
    }
}
