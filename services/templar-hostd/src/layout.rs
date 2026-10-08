//! Mounted deployment paths and boot identities are separate values from one layout.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

pub const INSTALLED_LAYOUT: &str = "usr/share/templaros/layout.toml";
const EMBEDDED: &str = include_str!("../../../distro/deploy/layout.toml");

#[derive(Debug, Deserialize)]
struct Declaration {
    version: u32,
    deployment: DeploymentDeclaration,
    state: StateDeclaration,
    esp: EspDeclaration,
}
#[derive(Debug, Deserialize)]
struct DeploymentDeclaration {
    mount: String,
    bootstrap_mount: String,
    subvolume_template: String,
    esp_dir_template: String,
}
#[derive(Debug, Deserialize)]
struct StateDeclaration { schema: u32, store_mount: String, path: Vec<StatePath> }
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct StatePath { source: String, target: String }
#[derive(Debug, Deserialize)]
struct EspDeclaration { mount: String }
#[derive(Deserialize)]
struct InstallPolicy { #[serde(default)] read_only: bool }

#[derive(Debug, Clone)]
pub struct DeploymentLayout {
    pub mounted_deployments: PathBuf,
    pub mounted_esp: PathBuf,
    pub mounted_bootstrap: PathBuf,
    pub mounted_state: PathBuf,
    state_paths: Vec<StatePath>,
    state_store_mount: String,
    pub subvolume_template: String,
    pub esp_dir_template: String,
    pub read_only: bool,
}

pub fn safe_subvolume(path: &str) -> bool {
    path.starts_with('@') && path.split('/').all(|part| !part.is_empty()
        && part != "." && part != ".."
        && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-')))
}
fn absolute_mount(path: &str) -> bool {
    path.starts_with('/') && path != "/" && !path.chars().any(char::is_control)
        && path.split('/').all(|part| part != "." && part != "..")
        && Path::new(path).components().all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

impl DeploymentLayout {
    pub fn read(root: &Path) -> Result<Self> {
        let declaration = match std::fs::read_to_string(root.join(INSTALLED_LAYOUT)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => EMBEDDED.to_owned(),
            Err(error) => return Err(error).context("reading deployment layout"),
        };
        Self::parse(root, &declaration)
    }
    pub fn parse(root: &Path, text: &str) -> Result<Self> {
        let declaration: Declaration = toml::from_str(text).context("parsing deployment layout")?;
        if declaration.version != 1 { bail!("unsupported deployment layout version {}", declaration.version); }
        if declaration.state.schema != crate::persistence::SCHEMA
            || !absolute_mount(&declaration.state.store_mount) || !absolute_mount(&declaration.esp.mount) {
            bail!("unsupported or unsafe shared-state/ESP layout");
        }
        let deployment = declaration.deployment;
        if !absolute_mount(&deployment.mount) { bail!("unsafe deployment mount {:?}", deployment.mount); }
        if !absolute_mount(&deployment.bootstrap_mount) { bail!("unsafe bootstrap mount"); }
        if deployment.subvolume_template.matches("{id}").count() != 1
            || !deployment.subvolume_template.ends_with("/{id}")
            || !safe_subvolume(&deployment.subvolume_template.replace("{id}", "candidate")) {
            bail!("unsafe deployment subvolume template {:?}", deployment.subvolume_template);
        }
        // Both shipped boot managers address this ESP namespace; its identity is never a host path.
        if deployment.esp_dir_template != "/EFI/templaros/{id}" { bail!("unsupported ESP deployment namespace"); }
        let install = root.join("etc/templaros/install.toml");
        let read_only = match std::fs::read_to_string(&install) {
            Ok(text) => toml::from_str::<InstallPolicy>(&text).context("parsing install policy")?.read_only,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error).context("reading install policy"),
        };
        Ok(Self {
            mounted_deployments: root.join(deployment.mount.trim_start_matches('/')),
            mounted_bootstrap: root.join(deployment.bootstrap_mount.trim_start_matches('/')),
            mounted_esp: root.join(declaration.esp.mount.trim_start_matches('/')),
            mounted_state: root.join(declaration.state.store_mount.trim_start_matches('/')),
            state_paths: declaration.state.path,
            state_store_mount: declaration.state.store_mount,
            subvolume_template: deployment.subvolume_template,
            esp_dir_template: deployment.esp_dir_template,
            read_only,
        })
    }
    pub fn candidate_mount(&self, id: &str) -> PathBuf { self.mounted_deployments.join(id) }
    pub fn subvolume(&self, id: &str) -> String { self.subvolume_template.replace("{id}", id) }
    pub fn esp_identity(&self, id: &str) -> String { self.esp_dir_template.replace("{id}", id) }
    pub fn mount_for_subvolume(&self, subvolume: &str) -> Result<PathBuf> {
        if subvolume == "@" { return Ok(self.mounted_bootstrap.clone()); }
        let (prefix, suffix) = self.subvolume_template.split_once("{id}").context("missing layout identity")?;
        let id = subvolume.strip_prefix(prefix).and_then(|id| id.strip_suffix(suffix))
            .filter(|id| crate::boot_entries::is_valid_deployment_id(id))
            .context("subvolume is not owned by the deployment layout")?;
        Ok(self.candidate_mount(id))
    }

    #[cfg(test)]
    pub fn fixture(mounted_deployments: PathBuf, mounted_esp: PathBuf) -> Self {
        Self { mounted_bootstrap: mounted_deployments.parent().unwrap_or(Path::new("/")).join("bootstrap"),
            mounted_state: mounted_deployments.parent().unwrap_or(Path::new("/")).join("state"),
            state_paths: Vec::new(),
            state_store_mount: "/var/lib/templaros/state".into(),
            mounted_deployments, mounted_esp, subvolume_template: "@deployments/{id}".into(),
            esp_dir_template: "/EFI/templaros/{id}".into(), read_only: true }
    }
    pub fn check_readers(&self, source: &Path, candidate: &Path) -> Result<()> {
        if !source.join(INSTALLED_LAYOUT).exists() { return Ok(()); }
        let marker: serde_json::Value = serde_json::from_slice(&std::fs::read(self.mounted_state.join(".templaros-state-schema.json"))
            .context("native root lacks its shared-state schema marker; reviewed offline migration required")?)?;
        if marker.get("schema").and_then(serde_json::Value::as_u64) != Some(u64::from(crate::persistence::SCHEMA)) {
            bail!("refusing unknown shared-state schema or blind downgrade");
        }
        if !candidate.join(INSTALLED_LAYOUT).exists() { bail!("native target lacks its deployment layout"); }
        let target = Self::read(candidate)?;
        if target.subvolume_template != self.subvolume_template || target.esp_dir_template != self.esp_dir_template
            || target.state_store_mount != self.state_store_mount || target.state_paths != self.state_paths
            || target.mounted_deployments.strip_prefix(candidate)? != self.mounted_deployments.strip_prefix(source)?
            || target.mounted_esp.strip_prefix(candidate)? != self.mounted_esp.strip_prefix(source)?
            || target.read_only != self.read_only {
            bail!("root switch changes deployment/State layout; reviewed offline migration required");
        }
        for root in [source, candidate] {
            let path = root.join("usr/share/templaros/state-compat.toml");
            let text = std::fs::read_to_string(path).context("fallback/candidate lacks the shared-state reader contract; reviewed offline migration required")?;
            let value: toml::Value = toml::from_str(&text)?;
            let minimum = value.get("minimum").and_then(toml::Value::as_integer).context("missing minimum state schema")?;
            let maximum = value.get("maximum").and_then(toml::Value::as_integer).context("missing maximum state schema")?;
            if minimum > i64::from(crate::persistence::SCHEMA) || maximum < i64::from(crate::persistence::SCHEMA) {
                bail!("candidate or fallback cannot read shared state schema");
            }
            if value.get("account_writer").and_then(toml::Value::as_integer) != Some(i64::from(crate::persistence::SCHEMA)) {
                bail!("native deployment lacks the owned recoverable account writer");
            }
        }
        Ok(())
    }

    pub fn preserve_gpu_mode(&self, source: &Path) -> Result<()> {
        use std::{fs, io::Write};
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if !source.join(INSTALLED_LAYOUT).exists() { return Ok(()); }
        let legacy = source.join("etc/templaros/hardware/gpu-mode");
        let store = self.mounted_state.join("var/lib/templaros/hardware");
        let destination = store.join("gpu-mode");
        if destination.try_exists()? || !legacy.try_exists()? { return Ok(()); }
        let info = fs::symlink_metadata(&legacy)?;
        if !info.is_file() || info.uid() != 0 { bail!("legacy GPU policy is not root-owned regular data"); }
        let mounted = fs::metadata(source.join("var/lib/templaros/hardware"))?;
        let backing = fs::metadata(&store)?;
        if mounted.dev() != backing.dev() || mounted.ino() != backing.ino() {
            bail!("GPU State is not bound to its declared store; reviewed offline migration required");
        }
        let temporary = store.join(".gpu-mode-migration");
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o644).open(&temporary)?;
        file.write_all(&fs::read(legacy)?)?;
        file.sync_all()?;
        let result = match fs::hard_link(&temporary, &destination) {
            Ok(()) => crate::persistence::sync_parent(&destination),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error).context("preserving GPU policy"),
        };
        fs::remove_file(temporary)?;
        result
    }
    /// Candidate fstab must follow the command-line subvolume, never remount the old @ root.
    pub fn prepare_candidate(&self, candidate: &Path, id: &str) -> Result<()> {
        let path = candidate.join("etc/fstab");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !candidate.join(INSTALLED_LAYOUT).exists() => return Ok(()),
            Err(error) => return Err(error).context("reading candidate fstab"),
        };
        let subvolume = self.subvolume(id);
        if candidate.join(INSTALLED_LAYOUT).exists() {
            let rows: Vec<Vec<&str>> = text.lines().filter(|line| !line.trim_start().starts_with('#'))
                .map(|line| line.split_whitespace().collect()).filter(|row: &Vec<&str>| row.len() >= 4).collect();
            let roots: Vec<_> = rows.iter().filter(|row| row[1] == "/").collect();
            if roots.len() != 1 || !roots[0][0].starts_with("UUID=") || rows.iter().any(|row| row[1] == "/var") {
                bail!("native candidate requires one UUID root and no blanket /var mount");
            }
            let store = &self.state_store_mount;
            for binding in &self.state_paths {
                let source = format!("{store}{}", binding.source);
                if !rows.iter().any(|row| row[0] == source && row[1] == binding.target && row[2] == "none" && row[3].split(',').any(|option| option == "bind")) {
                    bail!("candidate lacks declared persistent State bind {}", binding.target);
                }
            }
        }
        let mut output = String::with_capacity(text.len() + subvolume.len());
        for line in text.lines() {
            let mut fields: Vec<&str> = line.split_whitespace().collect();
            if !line.trim_start().starts_with('#') && fields.get(1) == Some(&"/") {
                if fields.get(2) != Some(&"btrfs") || fields.len() < 4 { bail!("candidate root is not declared btrfs"); }
                let mut options: Vec<&str> = fields[3].split(',')
                    .filter(|option| !option.starts_with("subvol=") && *option != "rw" && *option != "ro").collect();
                let identity = format!("subvol={subvolume}");
                options.push(&identity);
                options.push(if self.read_only { "ro" } else { "rw" });
                let options = options.join(",");
                fields[3] = &options;
                output.push_str(&fields.join("\t"));
            } else { output.push_str(line); }
            output.push('\n');
        }
        if output != text { std::fs::write(path, output).context("writing candidate fstab")?; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deployments_follow_the_layout() {
        let root = tempfile::tempdir().unwrap();
        let changed = EMBEDDED.replace("@deployments/{id}", "@candidates/{id}")
            .replace("mount = \"/var/lib/templaros/deployments\"", "mount = \"/candidates\"");
        let path = root.path().join(INSTALLED_LAYOUT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, changed).unwrap();
        let layout = DeploymentLayout::read(root.path()).unwrap();
        assert_eq!(layout.candidate_mount("txn-1"), root.path().join("candidates/txn-1"));
        assert_eq!(layout.subvolume("txn-1"), "@candidates/txn-1");
        assert_eq!(layout.esp_identity("txn-1"), "/EFI/templaros/txn-1");
        assert!(!safe_subvolume("@deployments/../other"));
        assert!(!safe_subvolume("/var/lib/templaros/deployments/txn-1"));
    }

    #[test]
    fn malformed_policy_and_future_layout_refuse_instead_of_assuming_writable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc/templaros")).unwrap();
        std::fs::write(root.path().join("etc/templaros/install.toml"), "read_only = \"true\"\n").unwrap();
        assert!(DeploymentLayout::parse(root.path(), EMBEDDED).is_err());
        std::fs::write(root.path().join("etc/templaros/install.toml"), "read_only = true\n").unwrap();
        assert!(DeploymentLayout::parse(root.path(), &EMBEDDED.replace("version = 1", "version = 2")).is_err());
        assert!(DeploymentLayout::parse(root.path(), &EMBEDDED.replace("schema = 1", "schema = 2")).is_err());
    }
}
