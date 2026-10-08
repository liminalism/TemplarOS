//! Event-driven, display-only session catalogue. Callers validate current managed instances
//! and process lifetimes; this evidence never creates application identity or provider authority.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};
use templar_types::{AppId, InstanceId};

const MAX_ROWS: usize = 256;
const SLOTS: usize = 32;
const MAX_JOURNAL: u64 = 16_384;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResumeV1 {
    pub adapter: String,
    pub session_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRowV1 {
    pub id: String,
    pub instance_id: Option<String>,
    pub kind: String,
    pub program: String,
    pub subject: String,
    pub state: String,
    pub updated_ms: u64,
    pub exit_status: Option<i32>,
    pub commands: Vec<String>,
    #[serde(with = "templar_types::native_path")]
    pub cwd: PathBuf,
    pub resume: Option<ResumeV1>,
}
/// Display-only process evidence from the caller, not canonical application identity.
/// Cross-cgroup descendants require lifetime/ancestry revalidation via `collect`.
#[derive(Clone, Debug)]
pub struct VerifiedTerminal {
    pub instance_id: InstanceId,
    pub pids: Vec<u32>,
    pub cwd: PathBuf,
}
#[derive(Clone, Debug)]
pub struct ResumeRequest {
    pub directory: PathBuf,
    pub argv: Vec<String>,
}
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct Catalogue {
    rows: BTreeMap<String, SessionRowV1>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema_version: u32,
    adapter: String,
    event: String,
    session_id: String,
    #[serde(with = "templar_types::native_path")]
    cwd: PathBuf,
}
#[derive(Deserialize)]
struct ClaudeInventoryEntry {
    #[serde(with = "templar_types::native_path")]
    cwd: PathBuf,
    kind: String,
    id: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    pid: Option<u32>,
    state: Option<String>,
    status: Option<String>,
}

impl Catalogue {
    /// Persisted presentation state is history, not proof of a process surviving daemon restart.
    /// A fresh verified process pass/public inventory will restore genuinely live rows.
    pub fn mark_resting(&mut self) {
        for row in self.rows.values_mut() {
            row.state = "ended".into();
        }
    }

    /// Agent hooks can run in any managed application, not just a terminal in `collect`.
    /// No stale or malformed instance identifier may continue to advertise current activity.
    pub fn reconcile_instances(&mut self, live: &BTreeSet<InstanceId>) {
        for row in self.rows.values_mut() {
            if row.instance_id.as_deref().is_some_and(|id| {
                InstanceId::parse(id).is_none_or(|instance| !live.contains(&instance))
            }) {
                row.state = "ended".into();
            }
        }
    }
    pub fn observe(
        &mut self,
        instance: InstanceId,
        _app_id: &AppId,
        observation_json: &str,
        now_ms: u64,
    ) -> Result<()> {
        if observation_json.len() > 8192 {
            bail!("session observation too large");
        }
        let observation: Observation = serde_json::from_str(observation_json)?;
        if observation.schema_version != 1 || observation.adapter != "claude-code" {
            bail!("unsupported session adapter/version");
        }
        if !safe_session_id(&observation.session_id) || !safe_directory(&observation.cwd) {
            bail!("invalid session id/directory");
        }
        let state = match observation.event.as_str() {
            "session-start" | "turn-start" => "running",
            "notification" => "waiting",
            "stop" => "idle",
            "session-end" => "ended",
            _ => bail!("unsupported session event"),
        };
        // Native IDs are used only as typed resume arguments; the UI identity is opaque.
        let existing = self
            .rows
            .values()
            .find(|row| {
                row.resume.as_ref().is_some_and(|resume| {
                    resume.adapter == observation.adapter
                        && resume.session_id == observation.session_id
                })
            })
            .map(|row| row.id.clone());
        let id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.rows.insert(
            id.clone(),
            SessionRowV1 {
                id,
                instance_id: Some(instance.to_string()),
                kind: "agent".into(),
                program: "claude".into(),
                subject: "Agent conversation".into(),
                state: state.into(),
                updated_ms: now_ms,
                exit_status: None,
                commands: Vec::new(),
                cwd: observation.cwd.canonicalize().unwrap_or(observation.cwd),
                resume: Some(ResumeV1 {
                    adapter: observation.adapter,
                    session_id: observation.session_id,
                }),
            },
        );
        self.bound();
        Ok(())
    }

    /// Public `claude agents --json --all --cwd` shape, documented in agent-view.
    /// Only projects that already exist in the ledger may be supplied. Public inventory
    /// never establishes an instance link; PID identity comes solely from the verified map.
    pub fn observe_claude_inventory(
        &mut self,
        project: &Path,
        json: &str,
        verified_pid_instances: &BTreeMap<u32, InstanceId>,
        now_ms: u64,
    ) -> Result<()> {
        if json.len() > 262_144 || !safe_directory(project) {
            bail!("invalid/bounded Claude inventory source");
        }
        let entries: Vec<ClaudeInventoryEntry> = serde_json::from_str(json)?;
        if entries.len() > MAX_ROWS {
            bail!("Claude inventory exceeds session bound");
        }
        let mut seen = BTreeSet::new();
        for entry in entries {
            if !safe_directory(&entry.cwd) {
                bail!("invalid Claude inventory directory");
            }
            let cwd = entry.cwd.canonicalize().unwrap_or(entry.cwd);
            if !cwd.is_absolute()
                || !cwd.starts_with(project)
                || cwd
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                bail!("Claude inventory directory outside requested project");
            }
            let (adapter, native_id) = if let Some(id) = entry.session_id.as_deref() {
                ("claude-code", id)
            } else if entry.kind == "background" {
                (
                    "claude-background",
                    entry
                        .id
                        .as_deref()
                        .context("Claude background inventory missing id")?,
                )
            } else {
                // Interactive sessions without a sessionId cannot be genuinely resumed.
                continue;
            };
            if !safe_session_id(native_id) {
                bail!("invalid Claude inventory resume identifier");
            }
            let state = match entry.state.as_deref() {
                Some("working") => "running",
                Some("blocked") => "waiting",
                Some("done") => "idle",
                Some("failed") => "failed",
                Some("stopped") => "ended",
                None => match entry.status.as_deref() {
                    Some("busy") => "running",
                    Some("waiting") => "waiting",
                    Some("idle") => "idle",
                    _ => bail!("Claude inventory lacks a supported live state"),
                },
                _ => bail!("unsupported Claude inventory state"),
            };
            let instance = entry
                .pid
                .and_then(|pid| verified_pid_instances.get(&pid))
                .map(ToString::to_string);
            let existing = self
                .rows
                .iter()
                .find(|(_, row)| {
                    row.resume.as_ref().is_some_and(|resume| {
                        resume.adapter == adapter && resume.session_id == native_id
                    })
                })
                .map(|(key, _)| key.clone());
            let key = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            seen.insert(key.clone());
            let row = self
                .rows
                .entry(key.clone())
                .or_insert_with(|| SessionRowV1 {
                    id: key,
                    instance_id: None,
                    kind: "agent".into(),
                    program: "claude".into(),
                    subject: "Agent conversation".into(),
                    state: state.into(),
                    updated_ms: now_ms,
                    exit_status: None,
                    commands: Vec::new(),
                    cwd: cwd.clone(),
                    resume: Some(ResumeV1 {
                        adapter: adapter.into(),
                        session_id: native_id.into(),
                    }),
                });
            if row.state != state || row.cwd != cwd {
                row.updated_ms = now_ms;
            }
            row.state = state.into();
            row.cwd = cwd;
            row.instance_id = instance;
        }
        // A successful public inventory is authoritative for its requested subtree. Missing
        // sessions are history, not forever-running or forever-waiting background processes.
        for (key, row) in &mut self.rows {
            if row.kind == "agent"
                && row.cwd.starts_with(project)
                && !seen.contains(key)
                && row.resume.as_ref().is_some_and(|resume| {
                    matches!(resume.adapter.as_str(), "claude-code" | "claude-background")
                })
            {
                row.state = "ended".into();
            }
        }
        self.bound();
        Ok(())
    }

    /// No timers: one bounded pass at a tending moment or verified window activation.
    /// A foreground group is accepted only when its member belongs to the supplied evidence.
    /// Read bounded candidate data, then revalidate kernel lifetime/current association before
    /// merging each row. The validator must cover both the shell and any foreground member.
    /// Failed evidence ends the old live row; no stale candidate is published.
    pub fn collect(
        &mut self,
        proc_root: &Path,
        runtime_dir: &Path,
        terminals: &[VerifiedTerminal],
        now_ms: u64,
        current: impl Fn(InstanceId, u32) -> bool,
    ) -> Result<()> {
        let mut live = BTreeSet::new();
        let mut errors = Vec::new();
        for terminal in terminals {
            let members: BTreeSet<_> = terminal.pids.iter().copied().collect();
            for pid in &terminal.pids {
                let base = proc_root.join(pid.to_string());
                let before = match read_stat(&base) {
                    Ok(stat) => stat,
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
                    {
                        continue;
                    }
                    Err(error) => {
                        errors.push(error.to_string());
                        continue;
                    }
                };
                // The controlling-terminal session leader identifies the shell context,
                // independent of shell names. Child job groups share this session ID.
                if before.tty == 0 || before.session != *pid as i32 || before.pgrp != *pid as i32 {
                    continue;
                }
                let leader = u32::try_from(before.tpgid).ok();
                let foreground = leader
                    .into_iter()
                    .chain(terminal.pids.iter().copied())
                    .filter(|member| members.contains(member))
                    .find_map(|member| {
                        let path = proc_root.join(member.to_string());
                        let stat = read_stat(&path).ok()?;
                        (stat.pgrp == before.tpgid).then_some((member, stat))
                    });
                let foreground_pid = foreground.as_ref().map(|(pid, _)| *pid);
                let mut cwd =
                    fs::read_link(base.join("cwd")).unwrap_or_else(|_| terminal.cwd.clone());
                let mut program = "shell".to_owned();
                let mut running = false;
                if let Some((foreground_pid, stat)) = foreground {
                    if stat.pgrp != before.pgrp {
                        running = true;
                        program = "command".into();
                        let path = proc_root.join(foreground_pid.to_string());
                        match bounded_read(&path.join("cmdline"), 8192) {
                            Ok(bytes) => {
                                let words: Vec<_> = bytes
                                    .split(|byte| *byte == 0)
                                    .filter_map(|word| std::str::from_utf8(word).ok())
                                    .collect();
                                let foreground_cwd = fs::read_link(path.join("cwd")).ok();
                                if read_stat(&path).is_ok_and(|after| {
                                    after.start == stat.start && after.pgrp == stat.pgrp
                                }) {
                                    program = safe_words(&words);
                                    if let Some(foreground_cwd) = foreground_cwd {
                                        cwd = foreground_cwd;
                                    }
                                }
                            }
                            Err(error)
                                if error
                                    .downcast_ref::<std::io::Error>()
                                    .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
                            {
                                ()
                            }
                            Err(error) => errors.push(format!(
                                "process {foreground_pid} foreground program unavailable: {error}"
                            )),
                        }
                    }
                }
                if !read_stat(&base)
                    .is_ok_and(|after| after.start == before.start && after.tpgid == before.tpgid)
                {
                    continue;
                }
                let prefix = runtime_dir
                    .join("templaros-garden")
                    .join(format!("shell-{pid}-{}", before.start));
                let mut entries = Vec::new();
                for slot in 0..SLOTS {
                    let path = prefix.with_extension(slot.to_string());
                    match read_journal(&path) {
                        Ok(Some(entry)) => entries.push(entry),
                        Ok(None) => (),
                        Err(error) => errors.push(format!("{}: {error}", path.display())),
                    }
                }
                if !read_stat(&base)
                    .is_ok_and(|after| after.start == before.start && after.tpgid == before.tpgid)
                    || !current(terminal.instance_id, *pid)
                    || foreground_pid.is_some_and(|pid| !current(terminal.instance_id, pid))
                {
                    continue;
                }
                let id_key = format!("{}:{pid}:{}", terminal.instance_id, before.start);
                // The internal key is stable only for this process lifetime; wire IDs disclose no PID.
                let row = self.rows.entry(id_key).or_insert_with(|| SessionRowV1 {
                    id: uuid::Uuid::new_v4().to_string(),
                    instance_id: Some(terminal.instance_id.to_string()),
                    kind: "terminal".into(),
                    program: "shell".into(),
                    subject: "Terminal session".into(),
                    state: "idle".into(),
                    updated_ms: now_ms,
                    exit_status: None,
                    commands: Vec::new(),
                    cwd: cwd.clone(),
                    resume: None,
                });
                live.insert(row.id.clone());
                row.cwd = cwd;
                row.program = program;
                row.state = if running { "running" } else { "idle" }.into();
                entries.sort_by_key(|entry| entry.sequence);
                row.commands = entries
                    .iter()
                    .rev()
                    .take(5)
                    .map(|entry| entry.label.clone())
                    .collect();
                row.commands.reverse();
                if let Some(entry) = entries.last() {
                    row.exit_status = Some(entry.exit_status);
                    row.updated_ms = now_ms.saturating_sub(
                        uptime_ms(proc_root)
                            .unwrap_or(entry.uptime_ms)
                            .saturating_sub(entry.uptime_ms),
                    );
                    if !running && entry.exit_status != 0 {
                        row.state = "failed".into();
                        row.program = entry.label.clone();
                    }
                } else {
                    row.updated_ms = now_ms;
                }
            }
        }
        let instances: BTreeSet<_> = terminals
            .iter()
            .map(|terminal| terminal.instance_id.to_string())
            .collect();
        let known_terminals: BTreeSet<_> = self
            .rows
            .values()
            .filter(|row| row.kind == "terminal")
            .filter_map(|row| row.instance_id.clone())
            .collect();
        for row in self.rows.values_mut() {
            if row.kind == "terminal" && !live.contains(&row.id) {
                row.state = "ended".into();
            }
            if row.kind == "agent"
                && row.instance_id.as_ref().is_some_and(|instance| {
                    known_terminals.contains(instance) && !instances.contains(instance)
                })
            {
                row.state = "ended".into();
            }
        }
        self.bound();
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("session collection: {}", errors.join("; "))
        }
    }
    pub fn rows(&self) -> Vec<SessionRowV1> {
        self.rows.values().cloned().collect()
    }
    pub fn resume(&self, id: &str) -> Option<ResumeRequest> {
        let row = self.rows.values().find(|row| row.id == id)?;
        if !safe_directory(&row.cwd) {
            return None;
        }
        let argv = match &row.resume {
            None => Vec::new(),
            Some(resume)
                if resume.adapter == "claude-code" && safe_session_id(&resume.session_id) =>
            {
                vec![
                    "claude".into(),
                    "--resume".into(),
                    resume.session_id.clone(),
                ]
            }
            Some(resume)
                if resume.adapter == "claude-background" && safe_session_id(&resume.session_id) =>
            {
                vec!["claude".into(), "attach".into(), resume.session_id.clone()]
            }
            Some(_) => return None,
        };
        Some(ResumeRequest {
            directory: row.cwd.clone(),
            argv,
        })
    }
    fn bound(&mut self) {
        while self.rows.len() > MAX_ROWS {
            let key = self
                .rows
                .iter()
                .min_by_key(|(_, row)| (row.state != "ended", row.updated_ms))
                .map(|(key, _)| key.clone());
            if let Some(key) = key {
                self.rows.remove(&key);
            }
        }
    }
}

#[derive(Debug)]
struct ProcessStat {
    pgrp: i32,
    session: i32,
    tty: i32,
    tpgid: i32,
    start: u64,
}
fn parse_stat(text: &str) -> Result<ProcessStat> {
    // comm may contain spaces, '(' and ')': the final ')' closes it, field 3 follows.
    let close = text.rfind(')').context("missing process comm terminator")?;
    let mut fields = text[close + 1..].split_whitespace();
    let pgrp = fields.nth(2).context("missing process group")?.parse()?;
    let session = fields.next().context("missing process session")?.parse()?;
    let tty = fields.next().context("missing process tty")?.parse()?;
    let tpgid = fields.next().context("missing foreground group")?.parse()?;
    // After field 8 (tpgid), skip fields 9–21 to read field 22 (starttime).
    let start = fields
        .nth(13)
        .context("missing process starttime")?
        .parse()?;
    Ok(ProcessStat {
        pgrp,
        session,
        tty,
        tpgid,
        start,
    })
}
fn read_stat(base: &Path) -> Result<ProcessStat> {
    parse_stat(&fs::read_to_string(base.join("stat"))?)
}
fn safe_session_id(id: &str) -> bool {
    id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
fn safe_directory(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
}
/// Never echo an arbitrary program/verb: argv[0] itself can be a secret or user text.
#[must_use]
pub fn safe_words(words: &[&str]) -> String {
    let mut words = words.iter().copied().skip_while(|word| {
        word.contains('=')
            || matches!(
                *word,
                "env" | "command" | "sudo" | "time" | "nice" | "nohup"
            )
    });
    let program = words
        .next()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let verb = words.next().unwrap_or("");
    let allowed = match program {
        "cargo" => &[
            "build", "test", "check", "run", "clippy", "fmt", "bench", "doc", "clean",
        ][..],
        "git" => &[
            "status", "diff", "log", "commit", "rebase", "merge", "fetch", "pull", "push",
            "checkout", "switch", "restore", "add",
        ][..],
        "npm" | "pnpm" | "yarn" => &["run", "test", "build", "install"][..],
        "claude" | "codex" | "gemini" | "bash" | "zsh" | "fish" | "sh" | "make" | "cmake"
        | "ninja" | "python" | "python3" | "node" | "go" | "rustc" | "nvim" | "vim" | "code"
        | "ls" | "cd" | "true" | "false" | "sleep" | "echo" | "printf" => &[][..],
        _ => return "command".into(),
    };
    if allowed.contains(&verb) {
        format!("{program} {verb}")
    } else {
        program.to_owned()
    }
}
fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("oversized session input");
    }
    Ok(bytes)
}
struct JournalEntry {
    sequence: u64,
    uptime_ms: u64,
    exit_status: i32,
    label: String,
}
fn read_journal(path: &Path) -> Result<Option<JournalEntry>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    let owner = fs::metadata(path.parent().context("journal has no directory")?)?.uid();
    if !meta.is_file()
        || meta.uid() != owner
        || meta.mode() & 0o077 != 0
        || meta.len() > MAX_JOURNAL
    {
        bail!("journal is not a private bounded regular file");
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_JOURNAL + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JOURNAL {
        bail!("journal grew beyond bound");
    }
    let text = std::str::from_utf8(&bytes)?;
    let fields: Vec<_> = text.trim_end_matches('\n').split('\t').collect();
    // Full-command opt-in stays local in column 9, never included in any wire DTO.
    if fields.len() != 9 || fields[0] != "1" {
        bail!("invalid journal record");
    }
    let _: u64 = fields[3].parse()?;
    let exit_status: i32 = fields[4].parse()?;
    if !(0..=255).contains(&exit_status) {
        bail!("invalid shell exit status");
    }
    for status in fields[5].split_whitespace() {
        let status: u16 = status.parse()?;
        if status > 255 {
            bail!("invalid pipeline status");
        }
    }
    let label = safe_words(&fields[7].split_whitespace().collect::<Vec<_>>());
    Ok(Some(JournalEntry {
        sequence: fields[1].parse()?,
        uptime_ms: fields[2].parse()?,
        exit_status: fields[4].parse()?,
        label,
    }))
}
fn uptime_ms(proc_root: &Path) -> Option<u64> {
    let text = fs::read_to_string(proc_root.join("uptime")).ok()?;
    let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some((seconds * 1000.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_handles_nested_parentheses_and_spaces() {
        let stat = parse_stat(concat!(
            "17 (a (b) c)) ",
            "S 1 17 17 34816 42 ", // state through foreground process group, fields 3–8
            "0 0 0 0 0 ",          // flags and fault counts, fields 9–13
            "0 0 0 0 ",            // process and child CPU times, fields 14–17
            "20 0 1 0 ",           // priority, nice, threads and obsolete timer, fields 18–21
            "12345 4096",          // starttime and virtual size, fields 22–23
        ))
        .unwrap();
        assert_eq!(stat.pgrp, 17);
        assert_eq!(stat.tpgid, 42);
        assert_eq!(stat.start, 12345);
        assert!(parse_stat("17 (gone)").is_err());
    }
    #[test]
    fn commands_never_echo_arguments_or_unapproved_verbs() {
        assert_eq!(
            safe_words(&["TOKEN=secret", "cargo", "test", "--token", "secret"]),
            "cargo test"
        );
        assert_eq!(safe_words(&["git", "secret-value"]), "git");
        assert_eq!(safe_words(&["/tmp/private-name", "secret"]), "command");
    }
    #[test]
    fn adapter_states_and_typed_resume_reject_executables_and_prompts() {
        let mut catalogue = Catalogue::default();
        let instance = InstanceId::new();
        let app: AppId = "org.example.Terminal".parse().unwrap();
        for (event, state) in [
            ("session-start", "running"),
            ("notification", "waiting"),
            ("stop", "idle"),
            ("session-end", "ended"),
        ] {
            let json = serde_json::json!({"schema_version":1,"adapter":"claude-code","event":event,"session_id":"abcd-1234","cwd":"/tmp/project"});
            catalogue
                .observe(instance.clone(), &app, &json.to_string(), 42)
                .unwrap();
            assert_eq!(catalogue.rows()[0].state, state);
        }
        let row = &catalogue.rows()[0];
        assert_eq!(
            catalogue.resume(&row.id).unwrap().argv,
            ["claude", "--resume", "abcd-1234"]
        );
        for extra in ["argv", "prompt", "message", "subject"] {
            let mut json = serde_json::json!({"schema_version":1,"adapter":"claude-code","event":"stop","session_id":"abcd","cwd":"/tmp"});
            json[extra] = serde_json::json!("secret");
            assert!(
                catalogue
                    .observe(instance.clone(), &app, &json.to_string(), 43)
                    .is_err()
            );
        }
    }

    #[test]
    fn native_cwds_survive_collection_persistence_wire_and_resume() {
        use std::os::unix::ffi::OsStringExt;
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let mut catalogue = Catalogue::default();
        let mut directories = BTreeSet::new();
        for byte in [0xfe, 0xff] {
            let cwd = project.join(std::ffi::OsString::from_vec(vec![b's', byte]));
            fs::create_dir(&cwd).unwrap();
            directories.insert(cwd.clone());
            let proc = temp.path().join(format!("proc-{byte}"));
            fs::create_dir(&proc).unwrap();
            process_fixture(&proc, 17, 17, 17, 100, "/bin/bash", &cwd, b"bash\0");
            let terminal = VerifiedTerminal {
                instance_id: InstanceId::new(),
                pids: vec![17],
                cwd,
            };
            catalogue
                .collect(&proc, temp.path(), &[terminal], 42, |_, _| true)
                .unwrap();
        }
        let persisted = serde_json::to_string(&catalogue).unwrap();
        assert!(persisted.contains("path_b64"));
        let restored: Catalogue = serde_json::from_str(&persisted).unwrap();
        let wire = serde_json::to_string(&restored.rows()).unwrap();
        let rows: Vec<SessionRowV1> = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.cwd.clone())
                .collect::<BTreeSet<_>>(),
            directories
        );
        assert_eq!(rows.len(), 2);
        for row in rows {
            let request = restored.resume(&row.id).unwrap();
            assert_eq!(request.directory, row.cwd);
            assert!(request.argv.is_empty());
            let uri = templar_types::uri_list::encode_path(&request.directory);
            assert_eq!(
                templar_types::uri_list::decode_uri(&uri),
                Some(request.directory)
            );
        }
    }
    #[test]
    fn private_journal_discards_opt_in_raw_column_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("0");
        fs::write(
            &path,
            "1\t2\t1000\t20\t1\t1 0\t/tmp\tcargo test\tPRIVATE_TOKEN\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let entry = read_journal(&path).unwrap().unwrap();
        assert_eq!(entry.label, "cargo test");
        assert_eq!(entry.exit_status, 1);
        symlink(&path, temp.path().join("link")).unwrap();
        assert!(read_journal(&temp.path().join("link")).is_err());
    }
    fn process_fixture(
        root: &Path,
        pid: u32,
        pgrp: u32,
        foreground: u32,
        start: u64,
        exe: &str,
        cwd: &Path,
        argv: &[u8],
    ) {
        use std::os::unix::fs::symlink;
        let directory = root.join(pid.to_string());
        fs::create_dir_all(&directory).unwrap();
        let mut fields = vec!["0".to_owned(); 20];
        fields[0] = "S".into();
        fields[2] = pgrp.to_string();
        fields[4] = "34816".into();
        fields[3] = "17".into();
        fields[5] = foreground.to_string();
        fields[19] = start.to_string();
        fs::write(
            directory.join("stat"),
            format!("{pid} (name (with) spaces)) {}", fields.join(" ")),
        )
        .unwrap();
        fs::write(directory.join("cmdline"), argv).unwrap();
        symlink(exe, directory.join("exe")).unwrap();
        symlink(cwd, directory.join("cwd")).unwrap();
    }
    #[test]
    fn foreground_requires_verified_group_and_resumes_last_cwd_after_exit() {
        let temp = tempfile::tempdir().unwrap();
        let proc = temp.path().join("proc");
        fs::create_dir(&proc).unwrap();
        process_fixture(
            &proc,
            17,
            17,
            42,
            100,
            "/bin/bash",
            Path::new("/tmp/project"),
            b"bash\0",
        );
        process_fixture(
            &proc,
            42,
            42,
            42,
            200,
            "/bin/cargo",
            Path::new("/tmp/project/sub"),
            b"cargo\0test\0--token\0SECRET\0",
        );
        let instance = InstanceId::new();
        let terminal = VerifiedTerminal {
            instance_id: instance,
            pids: vec![17, 42],
            cwd: "/tmp/project".into(),
        };
        let mut catalogue = Catalogue::default();
        catalogue
            .collect(&proc, temp.path(), &[terminal.clone()], 1000, |_, _| true)
            .unwrap();
        let row = catalogue.rows().pop().unwrap();
        assert_eq!(row.program, "cargo test");
        assert_eq!(row.state, "running");
        assert_eq!(row.cwd, Path::new("/tmp/project/sub"));
        assert!(!serde_json::to_string(&row).unwrap().contains("SECRET"));
        assert_eq!(
            catalogue.resume(&row.id).unwrap().argv,
            Vec::<String>::new()
        );
        // PID still exists, but belongs to no verified member: never use it as evidence.
        let unverified = VerifiedTerminal {
            pids: vec![17],
            ..terminal
        };
        catalogue
            .collect(&proc, temp.path(), &[unverified], 1100, |_, _| true)
            .unwrap();
        assert_eq!(catalogue.rows()[0].program, "shell");
        catalogue
            .collect(&proc, temp.path(), &[], 1200, |_, _| true)
            .unwrap();
        assert_eq!(catalogue.rows()[0].state, "ended");
    }
    #[test]
    fn journal_failure_time_and_process_churn_are_not_invented() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let proc = temp.path().join("proc");
        fs::create_dir(&proc).unwrap();
        fs::write(proc.join("uptime"), "2.00 0.00\n").unwrap();
        process_fixture(
            &proc,
            17,
            17,
            17,
            100,
            "/bin/bash",
            Path::new("/tmp/project"),
            b"bash\0",
        );
        let journals = temp.path().join("templaros-garden");
        fs::create_dir(&journals).unwrap();
        let journal = journals.join("shell-17-100.0");
        fs::write(
            &journal,
            "1\t1\t1500\t250\t1\t1 0\t/tmp/project\tcargo test\tSECRET\n",
        )
        .unwrap();
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
        let terminal = VerifiedTerminal {
            instance_id: InstanceId::new(),
            pids: vec![17],
            cwd: "/tmp/project".into(),
        };
        let mut catalogue = Catalogue::default();
        catalogue
            .collect(&proc, temp.path(), &[terminal.clone()], 10_000, |_, _| true)
            .unwrap();
        let row = &catalogue.rows()[0];
        assert_eq!(row.state, "failed");
        assert_eq!(row.exit_status, Some(1));
        assert_eq!(row.updated_ms, 9500);
        assert_eq!(row.commands, ["cargo test"]);
        fs::remove_dir_all(proc.join("17")).unwrap();
        catalogue
            .collect(&proc, temp.path(), &[terminal], 11_000, |_, _| true)
            .unwrap();
        assert_eq!(catalogue.rows()[0].state, "ended");
    }

    #[test]
    fn display_validation_gates_journals_foreground_wire_and_resume_without_replaying_commands() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let proc = temp.path().join("proc");
        fs::create_dir(&proc).unwrap();
        let cwd = temp.path().join("project");
        fs::create_dir(&cwd).unwrap();
        process_fixture(&proc, 17, 17, 17, 100, "/bin/bash", &cwd, b"bash\0");
        let journals = temp.path().join("templaros-garden");
        fs::create_dir(&journals).unwrap();
        let journal = journals.join("shell-17-100.0");
        fs::write(
            &journal,
            "1\t1\t1500\t250\t1\t1 0\t/tmp/project\tgit status\tPRIVATE\n",
        )
        .unwrap();
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
        let instance = InstanceId::new();
        let terminal = VerifiedTerminal {
            instance_id: instance,
            pids: vec![17],
            cwd: cwd.clone(),
        };
        let mut catalogue = Catalogue::default();
        // A PID/journal alone is never a consumer-visible session.
        catalogue
            .collect(&proc, temp.path(), &[terminal.clone()], 2000, |_, _| false)
            .unwrap();
        assert!(catalogue.rows().is_empty());
        catalogue
            .collect(&proc, temp.path(), &[terminal.clone()], 2001, |id, pid| {
                id == instance && pid == 17
            })
            .unwrap();
        let rows: Vec<SessionRowV1> =
            serde_json::from_str(&serde_json::to_string(&catalogue.rows()).unwrap()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].instance_id.as_deref(),
            Some(instance.to_string().as_str())
        );
        assert_eq!(rows[0].commands, ["git status"]);
        assert_eq!(rows[0].state, "failed");
        assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
        let resume = catalogue.resume(&rows[0].id).unwrap();
        assert_eq!(resume.directory, cwd);
        assert!(resume.argv.is_empty());
        // Once ancestry/lifetime is invalid, the prior row becomes history, not live evidence.
        catalogue
            .collect(&proc, temp.path(), &[terminal], 2002, |_, _| false)
            .unwrap();
        assert_eq!(catalogue.rows()[0].state, "ended");
        assert!(catalogue.resume(&rows[0].id).unwrap().argv.is_empty());
        // Foreground data must independently survive the same validation boundary.
        fs::remove_dir_all(proc.join("17")).unwrap();
        process_fixture(&proc, 17, 17, 42, 100, "/bin/bash", &cwd, b"bash\0");
        process_fixture(&proc, 42, 42, 42, 200, "/bin/cargo", &cwd, b"cargo\0test\0");
        let terminal = VerifiedTerminal {
            instance_id: instance,
            pids: vec![17, 42],
            cwd,
        };
        catalogue
            .collect(&proc, temp.path(), &[terminal.clone()], 2003, |_, pid| {
                pid == 17
            })
            .unwrap();
        assert_eq!(catalogue.rows()[0].state, "ended");
        catalogue
            .collect(&proc, temp.path(), &[terminal], 2004, |_, _| true)
            .unwrap();
        assert_eq!(catalogue.rows()[0].state, "running");
        assert_eq!(catalogue.rows()[0].program, "cargo test");
    }
    #[test]
    fn public_claude_inventory_projects_only_safe_states_and_declared_resume() {
        let mut catalogue = Catalogue::default();
        let inventory = r#"[{"cwd":"/tmp/project","kind":"background","startedAt":1,"id":"abc123","state":"blocked","name":"PRIVATE_PROMPT","summary":"PRIVATE_MESSAGE"},{"cwd":"/tmp/project/sub","kind":"interactive","startedAt":2,"sessionId":"safe-session","pid":42,"status":"busy","name":"SECRET_TITLE"}]"#;
        let instance = InstanceId::new();
        let verified = BTreeMap::from([(42, instance)]);
        catalogue
            .observe_claude_inventory(Path::new("/tmp/project"), inventory, &verified, 100)
            .unwrap();
        let rows = catalogue.rows();
        assert_eq!(rows.len(), 2);
        assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
        assert!(!serde_json::to_string(&rows).unwrap().contains("SECRET"));
        let background = rows.iter().find(|row| row.state == "waiting").unwrap();
        assert_eq!(background.instance_id, None);
        assert_eq!(
            catalogue.resume(&background.id).unwrap().argv,
            ["claude", "attach", "abc123"]
        );
        let live = rows.iter().find(|row| row.state == "running").unwrap();
        assert_eq!(live.instance_id, Some(instance.to_string()));
        catalogue
            .observe_claude_inventory(Path::new("/tmp/project"), inventory, &verified, 200)
            .unwrap();
        assert!(catalogue.rows().iter().all(|row| row.updated_ms == 100));
        assert!(
            catalogue
                .observe_claude_inventory(Path::new("/other"), inventory, &verified, 300)
                .is_err()
        );
        assert!(catalogue.observe_claude_inventory(Path::new("/tmp/project"), r#"[{"cwd":"/tmp/project","kind":"background","id":"--evil","state":"blocked"}]"#, &verified, 300).is_err());
        catalogue
            .observe_claude_inventory(Path::new("/tmp/project"), inventory, &BTreeMap::new(), 400)
            .unwrap();
        assert!(catalogue.rows().iter().all(|row| row.instance_id.is_none()));
        catalogue
            .observe_claude_inventory(Path::new("/tmp/project"), "[]", &verified, 500)
            .unwrap();
        assert!(catalogue.rows().iter().all(|row| row.state == "ended"));
        catalogue
            .observe_claude_inventory(Path::new("/tmp/project"), inventory, &verified, 600)
            .unwrap();
        catalogue.reconcile_instances(&BTreeSet::new());
        assert_eq!(
            catalogue
                .rows()
                .iter()
                .find(|row| row.instance_id.is_some())
                .unwrap()
                .state,
            "ended"
        );
        catalogue.mark_resting();
        assert!(catalogue.rows().iter().all(|row| row.state == "ended"));
    }
}
