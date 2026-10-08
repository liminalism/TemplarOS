//! Declaration gate, not a claim that Unix permissions enforce these domains.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Tier {
    System,
    State,
    Work,
}
#[derive(Debug, Deserialize)]
struct Exception {
    path: String,
    decision: String,
    reason: String,
    #[serde(default)]
    tier: Option<Tier>,
}
#[derive(Debug, Deserialize)]
struct Region {
    #[serde(alias = "target")]
    path: String,
    tier: Option<Tier>,
    owner: Option<String>,
    writes: Option<Vec<String>>,
    exceptions: Option<Vec<Exception>>,
}
#[derive(Debug, Deserialize)]
struct Component {
    name: String,
    tier: Option<Tier>,
    owner: Option<String>,
    writes: Option<Vec<String>>,
    exceptions: Option<Vec<Exception>>,
    unit: Option<String>,
    #[serde(default)]
    additional_units: Vec<String>,
    scope: Option<String>,
}
#[derive(Debug, Deserialize)]
struct Matrix {
    component: Vec<Component>,
}
#[derive(Debug, Deserialize)]
struct Ownership {
    version: u32,
    external_owners: Vec<String>,
    subvolume: Vec<Region>,
    path: Vec<Region>,
    updater: Vec<Region>,
}
#[derive(Debug, Deserialize)]
struct Root {
    subvolumes: Vec<String>,
}
#[derive(Debug, Deserialize)]
struct Layout {
    root: Root,
    ownership: Ownership,
    state: Option<State>,
}
#[derive(Debug, Deserialize)]
struct State {
    schema: u32,
    path: Vec<Region>,
}

fn clean_path(path: &str) -> bool {
    (path.starts_with('/')
        || path.starts_with("%h/")
        || path.starts_with("%t/")
        || path == "%h"
        || path == "%t")
        && !path.contains(['\n', '\r', '\0'])
        && path.split('/').all(|part| part != "." && part != "..")
}
fn expand(path: &str, user: bool) -> String {
    path.trim_start_matches(['-', '+'])
        .replace("%S", if user { "%h/.local/state" } else { "/var/lib" })
        .replace("%C", if user { "%h/.cache" } else { "/var/cache" })
}
fn covers(region: &str, path: &str) -> bool {
    let prefix = region.trim_end_matches("/**");
    path == prefix
        || (path.starts_with(prefix)
            && (prefix == "/" || path.as_bytes().get(prefix.len()) == Some(&b'/')))
}
fn exception_matches(region: &str, path: &str) -> bool {
    if region.ends_with("/**") {
        covers(region, path)
    } else if let Some(prefix) = region.strip_suffix('*') {
        path.starts_with(prefix)
    } else {
        region == path
    }
}
fn validate_exceptions(
    exceptions: Option<&[Exception]>,
    subject: &str,
    problems: &mut Vec<String>,
) {
    let Some(exceptions) = exceptions else {
        problems.push(format!("{subject}: missing exceptions (use [] explicitly)"));
        return;
    };
    for exception in exceptions {
        let decision = exception
            .decision
            .strip_prefix("@legeos.decision.")
            .or_else(|| exception.decision.strip_prefix("@templaros.decision."));
        if !clean_path(&exception.path)
            || exception.reason.trim().is_empty()
            || !decision.is_some_and(|key| {
                key.rsplit_once('/').is_some_and(|(name, revision)| {
                    !name.is_empty() && revision.parse::<u32>().is_ok_and(|revision| revision > 0)
                })
            })
        {
            problems.push(format!("{subject}: invalid exception for {:?}; path, decision revision and reason required", exception.path));
        }
    }
}

fn decision_problem(reference: &str, envelope: &serde_json::Value) -> Option<String> {
    let Some((key, revision)) = reference
        .strip_prefix('@')
        .and_then(|key| key.rsplit_once('/'))
    else {
        return Some(format!("{reference}: malformed pinned decision reference"));
    };
    let Ok(revision) = revision.parse::<u64>() else {
        return Some(format!("{reference}: malformed pinned decision revision"));
    };
    let result = &envelope["result"];
    if envelope["ok"] != true
        || envelope["exit_code"] != 0
        || result["key"] != key
        || result["rev"] != revision
        || result["kind"] != "decision"
        || result["class"] != "normative"
    {
        return Some(format!(
            "{reference}: missing or mismatched pinned normative decision"
        ));
    }
    if result["state"] != "active" || result["is_head"] != true {
        return Some(format!(
            "{reference}: {:?} decision is not active installed/release authority; proposed source-candidates require explicit reconciliation",
            result["state"]
        ));
    }
    if result["freshness"]["stale"] != false || result["freshness"]["at_risk"] != false {
        return Some(format!(
            "{reference}: decision provenance needs reconciliation (not a source-regression claim)"
        ));
    }
    None
}

fn resolve_decisions(
    root: &Path,
    layout: &Layout,
    matrix: &Matrix,
    problems: &mut Vec<String>,
) -> Result<()> {
    let references: BTreeSet<_> = layout
        .ownership
        .subvolume
        .iter()
        .chain(&layout.ownership.path)
        .chain(&layout.ownership.updater)
        .chain(layout.state.iter().flat_map(|state| &state.path))
        .flat_map(|region| region.exceptions.iter().flatten())
        .chain(
            matrix
                .component
                .iter()
                .flat_map(|component| component.exceptions.iter().flatten()),
        )
        .map(|exception| exception.decision.as_str())
        .collect();
    for reference in references {
        let output = std::process::Command::new("akr").current_dir(root)
            .args(["--format", "json", "get", reference]).output()
            .context("check-ownership requires the public akr executable; install/configure it and reconcile pinned ownership decisions before release")?;
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("reading public AKR result for {reference}"))?;
        if !output.status.success() {
            problems.push(format!(
                "{reference}: AKR resolution failed; diagnostics={}",
                envelope["diagnostics"]
            ));
        } else if let Some(problem) = decision_problem(reference, &envelope) {
            problems.push(problem);
        }
    }
    Ok(())
}
fn check_region(region: &Region, owners: &BTreeSet<&str>, problems: &mut Vec<String>) {
    if region.tier.is_none()
        || !region
            .owner
            .as_deref()
            .is_some_and(|owner| owners.contains(owner))
        || region.writes.is_none()
    {
        problems.push(format!(
            "{}: missing/unknown tier, owner or writes",
            region.path
        ));
    }
    validate_exceptions(region.exceptions.as_deref(), &region.path, problems);
}
fn write_problem(
    layout: &Layout,
    owner: &str,
    path: &str,
    exceptions: &[Exception],
) -> Option<String> {
    if !clean_path(path) {
        return Some(format!("unsafe or unclassified write {path:?}"));
    }
    let region = layout
        .ownership
        .path
        .iter()
        .chain(layout.state.iter().flat_map(|state| &state.path))
        .filter(|region| covers(&region.path, path))
        .max_by_key(|region| region.path.len());
    let Some(region) = region else {
        return Some(format!("undeclared write {path}"));
    };
    if region.owner.as_deref() == Some(owner) {
        return None;
    }
    if exceptions.iter().any(|exception| {
        exception_matches(&exception.path, path)
            && exception.tier.is_none_or(|tier| region.tier == Some(tier))
    }) {
        return None;
    }
    Some(format!(
        "{owner}: write {path} crosses {:?} owned by {:?} without a decision exception",
        region.tier, region.owner
    ))
}
fn declarations(layout: &Layout, matrix: &Matrix) -> Vec<String> {
    let mut problems = Vec::new();
    if layout.ownership.version != 1 {
        problems.push("unsupported ownership schema".into());
    }
    let owners: BTreeSet<_> = matrix
        .component
        .iter()
        .filter_map(|component| component.owner.as_deref())
        .chain(layout.ownership.external_owners.iter().map(String::as_str))
        .collect();
    for region in layout
        .ownership
        .subvolume
        .iter()
        .chain(&layout.ownership.path)
        .chain(&layout.ownership.updater)
    {
        check_region(region, &owners, &mut problems);
    }
    if let Some(state) = &layout.state {
        if state.schema != 1 {
            problems.push("unsupported shared-state schema".into());
        }
        for region in &state.path {
            check_region(region, &owners, &mut problems);
            if region.tier != Some(Tier::State)
                || !clean_path(&region.path)
                || ["/", "/var", "/etc", "/home", "/data"].contains(&region.path.as_str())
                || ["/var/lib/dpkg", "/var/lib/apt", "/var/cache/apt"]
                    .iter()
                    .any(|path| covers(path, &region.path) || covers(&region.path, path))
                || state.path.iter().any(|other| {
                    other.path != region.path
                        && (covers(&other.path, &region.path) || covers(&region.path, &other.path))
                })
            {
                problems.push(format!(
                    "{}: invalid blanket, Work or package-state bind",
                    region.path
                ));
            }
        }
    }
    for path in &layout.root.subvolumes {
        let regions: Vec<_> = layout
            .ownership
            .subvolume
            .iter()
            .filter(|region| &region.path == path)
            .collect();
        if regions.len() != 1 {
            problems.push(format!(
                "subvolume {path}: exactly one tier/owner declaration required"
            ));
        }
    }
    for component in &matrix.component {
        if component.tier.is_none()
            || component
                .owner
                .as_deref()
                .is_none_or(|owner| owner.trim().is_empty())
            || component.writes.is_none()
        {
            problems.push(format!("{}: missing tier, owner or writes", component.name));
        }
        validate_exceptions(
            component.exceptions.as_deref(),
            &component.name,
            &mut problems,
        );
        let owner = component.owner.as_deref().unwrap_or("");
        for path in component.writes.as_deref().unwrap_or(&[]) {
            if let Some(problem) = write_problem(
                layout,
                owner,
                &expand(path, true),
                component.exceptions.as_deref().unwrap_or(&[]),
            ) {
                problems.push(problem);
            }
        }
    }
    for updater in &layout.ownership.updater {
        for path in updater.writes.as_deref().unwrap_or(&[]) {
            if let Some(problem) = write_problem(
                layout,
                updater.owner.as_deref().unwrap_or(""),
                path,
                updater.exceptions.as_deref().unwrap_or(&[]),
            ) {
                problems.push(problem);
            }
        }
    }
    problems
}
fn unit_problems(layout: &Layout, component: &Component, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let user = component.scope.as_deref() == Some("user");
    let owner = component.owner.as_deref().unwrap_or("");
    let writes = component.writes.as_deref().unwrap_or(&[]);
    let exceptions = component.exceptions.as_deref().unwrap_or(&[]);
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
    {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        for entry in value.split_whitespace() {
            let path = match key {
                "StateDirectory" => expand(&format!("%S/{entry}"), user),
                "CacheDirectory" => expand(&format!("%C/{entry}"), user),
                "RuntimeDirectory" => {
                    if user {
                        format!("%t/{entry}")
                    } else {
                        format!("/run/{entry}")
                    }
                }
                "ReadWritePaths" => expand(entry, user),
                _ => continue,
            };
            let declared = writes
                .iter()
                .any(|write| covers(&expand(write, user), &path))
                || exceptions
                    .iter()
                    .any(|exception| exception_matches(&exception.path, &path));
            if !declared {
                problems.push(format!(
                    "{}: unit {key}={entry} is not a declared write",
                    component.name
                ));
            }
            if let Some(problem) = write_problem(layout, owner, &path, exceptions) {
                problems.push(problem);
            }
        }
    }
    problems
}
fn package_problem(layout: &Layout, package: &str, destination: &str) -> Option<String> {
    let path = format!("/{}", destination.trim_start_matches('/'));
    let region = layout
        .ownership
        .path
        .iter()
        .chain(layout.state.iter().flat_map(|state| &state.path))
        .filter(|region| covers(&region.path, &path))
        .max_by_key(|region| region.path.len());
    if !clean_path(&path) || region.is_none_or(|region| region.tier != Some(Tier::System)) {
        Some(format!(
            "{package}: package asset {path} installs outside the system tier"
        ))
    } else {
        None
    }
}

pub fn check(root: &Path) -> Result<()> {
    let layout: Layout = toml::from_str(&std::fs::read_to_string(
        root.join("distro/deploy/layout.toml"),
    )?)?;
    let matrix: Matrix = toml::from_str(&std::fs::read_to_string(
        root.join("distro/deploy/components.toml"),
    )?)?;
    let mut problems = declarations(&layout, &matrix);
    resolve_decisions(root, &layout, &matrix, &mut problems)?;
    for component in &matrix.component {
        for unit in component.unit.iter().chain(&component.additional_units) {
            let scope = component.scope.as_deref().unwrap_or("none");
            let path = root.join(format!("session/units/{scope}/{unit}"));
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading ownership unit {}", path.display()))?;
            problems.extend(unit_problems(&layout, component, &text));
        }
    }
    let metadata = crate::cargo_metadata(root, &["--no-deps"])?;
    if let Some(packages) = metadata["packages"].as_array() {
        for package in packages {
            let name = package["name"].as_str().unwrap_or("?");
            if let Some(assets) = package["metadata"]["deb"]["assets"].as_array() {
                for asset in assets {
                    if let Some(destination) = asset.get(1).and_then(serde_json::Value::as_str)
                        && let Some(problem) = package_problem(&layout, name, destination)
                    {
                        problems.push(problem);
                    }
                }
            }
        }
    }
    // New apt-writing scripts must be reviewed, not disappear into the legacy exception.
    for directory in ["scripts", "installer"] {
        for entry in std::fs::read_dir(root.join(directory))? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "sh" || extension == "py")
            {
                let text = std::fs::read_to_string(&path)?;
                if text.contains("apt-get") || text.contains("dpkg -i") || text.contains("chpasswd")
                {
                    let relative = path.strip_prefix(root)?.to_string_lossy();
                    if !layout
                        .ownership
                        .updater
                        .iter()
                        .any(|updater| updater.path == relative)
                    {
                        problems.push(format!("{relative}: undeclared package-writing updater"));
                    }
                }
            }
        }
    }
    for updater in &layout.ownership.updater {
        if !root.join(&updater.path).is_file() {
            problems.push(format!("missing declared updater {}", updater.path));
        }
    }
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("check-ownership: {problem}");
        }
        bail!("{} ownership declaration failure(s)", problems.len());
    }
    println!(
        "check-ownership: {} components, {} subvolumes and {} path domains checked; declaration aid, not runtime confinement",
        matrix.component.len(),
        layout.ownership.subvolume.len(),
        layout.ownership.path.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Layout, Matrix) {
        let layout = toml::from_str(
            r#"
[root]
subvolumes = ["/@", "/@home"]
[ownership]
version = 1
external_owners = ["user"]
[[ownership.subvolume]]
path = "/@"
tier = "system"
owner = "hostd"
writes = ["/usr"]
exceptions = []
[[ownership.subvolume]]
path = "/@home"
tier = "work"
owner = "user"
writes = ["/home"]
exceptions = []
[[ownership.path]]
path = "/"
tier = "system"
owner = "hostd"
writes = []
exceptions = []
[[ownership.path]]
path = "/home"
tier = "work"
owner = "user"
writes = []
exceptions = []
[[ownership.path]]
path = "/var/lib/hostd"
tier = "state"
owner = "hostd"
writes = []
exceptions = []
[[ownership.path]]
path = "/var/lib/other"
tier = "state"
owner = "other"
writes = []
exceptions = []
[[ownership.updater]]
path = "updater.sh"
tier = "system"
owner = "hostd"
writes = ["/usr"]
exceptions = []
"#,
        )
        .unwrap();
        let matrix = toml::from_str(
            r#"
[[component]]
name = "hostd"
tier = "system"
owner = "hostd"
writes = ["/var/lib/hostd"]
exceptions = []
scope = "system"
[[component]]
name = "other"
tier = "system"
owner = "other"
writes = []
exceptions = []
"#,
        )
        .unwrap();
        (layout, matrix)
    }
    #[test]
    fn ownership_rejects_missing_owner_work_packages_foreign_permissions_and_empty_decisions() {
        let (layout, mut matrix) = fixture();
        assert!(declarations(&layout, &matrix).is_empty());
        assert!(unit_problems(&layout, &matrix.component[0], "StateDirectory=hostd").is_empty());
        assert!(package_problem(&layout, "good", "usr/bin/good").is_none());
        assert!(package_problem(&layout, "bad", "home/user/bin/bad").is_some());
        assert!(
            !unit_problems(
                &layout,
                &matrix.component[0],
                "ReadWritePaths=/var/lib/other"
            )
            .is_empty()
        );
        matrix.component[0].owner = None;
        assert!(!declarations(&layout, &matrix).is_empty());
        matrix.component[0].owner = Some("hostd".into());
        matrix.component[0].exceptions = Some(vec![Exception {
            path: "/home/**".into(),
            decision: String::new(),
            reason: "test".into(),
            tier: None,
        }]);
        assert!(!declarations(&layout, &matrix).is_empty());
        matrix.component[0].exceptions = Some(Vec::new());
        matrix.component[0]
            .writes
            .as_mut()
            .unwrap()
            .push("/home/user/work".into());
        assert!(!declarations(&layout, &matrix).is_empty());
    }

    #[test]
    fn decision_resolution_requires_exact_active_fresh_normative_revision() {
        let reference = "@legeos.decision.test/2";
        let mut envelope = serde_json::json!({"ok":true,"exit_code":0,"result":{
            "key":"legeos.decision.test","rev":2,"kind":"decision","class":"normative",
            "state":"active","is_head":true,"freshness":{"stale":false,"at_risk":false}}});
        assert!(decision_problem(reference, &envelope).is_none());
        for state in ["proposed", "retired", "superseded"] {
            envelope["result"]["state"] = serde_json::json!(state);
            assert!(decision_problem(reference, &envelope).is_some());
        }
        envelope["result"]["state"] = serde_json::json!("active");
        envelope["result"]["rev"] = serde_json::json!(3);
        assert!(decision_problem(reference, &envelope).is_some());
        envelope["result"]["rev"] = serde_json::json!(2);
        envelope["result"]["freshness"]["at_risk"] = serde_json::json!(true);
        assert!(decision_problem(reference, &envelope).is_some());
    }
}
