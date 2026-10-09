//! Typed actions over the JSON Wallpaper1 wire contract. No client-side policy or timers.
use crate::proxies::WallpaperProxy;
use templar_types::wallpaper::WallpaperStatus;

#[derive(Debug)]
pub struct WallpaperClient<'a> {
    proxy: WallpaperProxy<'a>,
}
impl<'a> WallpaperClient<'a> {
    pub async fn new(connection: &'a zbus::Connection) -> anyhow::Result<Self> {
        Ok(Self {
            proxy: WallpaperProxy::new(connection).await?,
        })
    }
    pub async fn inspect(&self) -> anyhow::Result<WallpaperStatus> {
        Ok(serde_json::from_str(&self.proxy.inspect().await?)?)
    }
    pub async fn next(&self) -> anyhow::Result<WallpaperStatus> {
        Ok(serde_json::from_str(&self.proxy.next().await?)?)
    }
    pub async fn previous(&self) -> anyhow::Result<WallpaperStatus> {
        Ok(serde_json::from_str(&self.proxy.previous().await?)?)
    }
    pub async fn pause(&self) -> anyhow::Result<WallpaperStatus> {
        Ok(serde_json::from_str(&self.proxy.pause().await?)?)
    }
    pub async fn resume(&self) -> anyhow::Result<WallpaperStatus> {
        Ok(serde_json::from_str(&self.proxy.resume().await?)?)
    }
    pub async fn refresh(&self) -> anyhow::Result<WallpaperStatus> {
        Ok(serde_json::from_str(&self.proxy.refresh().await?)?)
    }
    pub fn proxy(&self) -> &WallpaperProxy<'a> {
        &self.proxy
    }
}
