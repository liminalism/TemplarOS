//! Drives drawer: `org.templaros.Files.Volumes1.List` — every user-visible volume with its free
//! space and the system reserve.
//!
//! `docs/architecture/files.md` makes this the point of the interface: the sidebar, the Drives
//! drawer and the file chooser all render *this one list*, so none of them can show a volume
//! without also showing how full it is, and nobody has to open a disk utility to discover a drive
//! is nearly full. The reserve is rendered alongside free space rather than folded into it —
//! space kept back for logging in, updating and recovering is not space the user can spend.

use crate::drawers::DrawerFeed;
use crate::format;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;
use templar_client::proxies::FilesVolumesProxy;

/// Mirrors `services/templar-filed/src/storage.rs::VolumeInfo`'s wire JSON. `templar-filed` is not a
/// dependency of the shell (a restartable UI process has no business linking rusqlite/libc for
/// type reuse), so this is the wire contract, kept in step with the source by hand.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct VolumeInfo {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    /// Space the OS keeps back for login, update and recovery.
    pub reserved_bytes: u64,
    pub read_only: bool,
}

impl VolumeInfo {
    /// What applications may still consume before they would eat into the reserve. Saturating,
    /// so a volume already inside its reserve reports nothing usable rather than underflowing.
    pub fn usable_bytes(&self) -> u64 {
        self.available_bytes.saturating_sub(self.reserved_bytes)
    }

    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }
}

/// What the Drives drawer needs from `Files.Volumes1`, behind a trait so tests never touch a
/// session bus.
#[async_trait]
pub trait VolumesClient: Send + Sync {
    /// Raw `volumes_json` — parsing stays in the renderer so a fake can hand back exactly the
    /// bytes the service would.
    async fn list(&self) -> Result<String>;
}

#[async_trait]
impl VolumesClient for FilesVolumesProxy<'_> {
    async fn list(&self) -> Result<String> {
        self.list().await.context("Files.Volumes1.List")
    }
}

pub async fn render(client: &impl VolumesClient) -> Result<String> {
    Ok(feed(client).await?.to_text())
}

pub async fn feed(client: &impl VolumesClient) -> Result<DrawerFeed> {
    feed_json(&client.list().await?)
}

fn feed_json(json: &str) -> Result<DrawerFeed> {
    let volumes: Vec<VolumeInfo> =
        serde_json::from_str(json).context("parsing Files.Volumes1.List volumes_json")?;
    if volumes.is_empty() {
        return Ok(DrawerFeed::vacant("no volumes"));
    }
    // The glance is how much room is left, not how many disks there are: a count of volumes is
    // never the reason anyone looks at this drawer.
    let free: u64 = volumes.iter().map(VolumeInfo::usable_bytes).sum();
    // Terse on purpose: the tile's own band already says DRIVES, and a compact tile has room
    // for the figure or the sentence, not both.
    let headline = format!("{} usable", format::bytes(free));
    let lines = volumes
        .iter()
        .map(|volume| {
            format!(
                "{:<20} {:<8} {} free of {} ({}% used, {} reserved, {} usable){}",
                volume.mount_point.display(),
                volume.fs_type,
                format::bytes(volume.free_bytes),
                format::bytes(volume.total_bytes),
                format::percent(volume.used_bytes(), volume.total_bytes),
                format::bytes(volume.reserved_bytes),
                format::bytes(volume.usable_bytes()),
                if volume.read_only { "  read-only" } else { "" },
            )
        })
        .collect();
    Ok(DrawerFeed::new(headline, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeVolumes(&'static str);

    #[async_trait]
    impl VolumesClient for FakeVolumes {
        async fn list(&self) -> Result<String> {
            Ok(self.0.to_owned())
        }
    }

    const FIXTURE: &str = r#"[
        {"mount_point":"/","fs_type":"btrfs","total_bytes":1073741824,"free_bytes":536870912,
         "available_bytes":536870912,"reserved_bytes":268435456,"read_only":false},
        {"mount_point":"/boot/efi","fs_type":"vfat","total_bytes":536870912,"free_bytes":268435456,
         "available_bytes":268435456,"reserved_bytes":0,"read_only":true}
    ]"#;

    #[test]
    fn a_service_shaped_fixture_parses_into_the_exact_fields() {
        let volumes: Vec<VolumeInfo> = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].fs_type, "btrfs");
        assert!(!volumes[0].read_only);
        assert!(volumes[1].read_only);
    }

    #[test]
    fn usable_space_excludes_the_reserve_and_never_underflows() {
        let volumes: Vec<VolumeInfo> = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(volumes[0].usable_bytes(), 268_435_456);

        let inside_reserve = VolumeInfo {
            mount_point: "/".into(),
            fs_type: "btrfs".into(),
            total_bytes: 100,
            free_bytes: 10,
            available_bytes: 10,
            reserved_bytes: 50,
            read_only: false,
        };
        assert_eq!(inside_reserve.usable_bytes(), 0);
    }

    #[tokio::test]
    async fn every_volume_shows_free_space_and_its_reserve() {
        let feed = feed(&FakeVolumes(FIXTURE)).await.unwrap();
        assert_eq!(feed.lines.len(), 2);
        assert!(feed.lines[0].contains("512.0 MiB free of 1.0 GiB"));
        assert!(feed.lines[0].contains("50% used"));
        assert!(feed.lines[0].contains("256.0 MiB reserved"));
        assert!(feed.lines[0].contains("256.0 MiB usable"));
        assert!(feed.lines[1].contains("read-only"));
        // The rendered text is the headline and then those lines.
        assert_eq!(
            render(&FakeVolumes(FIXTURE)).await.unwrap().lines().count(),
            3
        );
    }

    /// The glance is room left, not a count of disks: nobody opens this drawer to learn how many
    /// volumes they have.
    #[tokio::test]
    async fn the_headline_is_the_usable_space_across_every_volume() {
        let full = feed(&FakeVolumes(FIXTURE)).await.unwrap();
        assert_eq!(full.headline, "512.0 MiB usable");
        assert!(full.available);

        let empty = feed(&FakeVolumes("[]")).await.unwrap();
        assert_eq!(empty.headline, "no volumes");
        assert!(empty.lines.is_empty());
    }

    #[tokio::test]
    async fn no_volumes_is_said_plainly() {
        assert_eq!(render(&FakeVolumes("[]")).await.unwrap(), "no volumes");
    }

    #[tokio::test]
    async fn malformed_json_is_a_clear_error_not_a_panic() {
        let error = render(&FakeVolumes("not json")).await.unwrap_err();
        assert!(error.to_string().contains("Files.Volumes1.List"));
    }
}
