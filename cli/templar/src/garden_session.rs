//! Local journal setup and supported public agent hook inputs. No private agent-store reads.
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::PathBuf,
};

/// Once per shell startup; prompt writes thereafter use shell builtins only.
pub fn journal_init(pid: u32) -> Result<PathBuf> {
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?);
    let metadata = fs::symlink_metadata(&runtime)?;
    let uid = rustix::process::getuid().as_raw();
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!("runtime directory must be owned and private");
    }
    let process = fs::metadata(format!("/proc/{pid}"))?;
    if process.uid() != uid {
        bail!("shell is not owned by this user");
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat.rfind(')').context("invalid shell process stat")?;
    let start: u64 = stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .context("short shell stat")?
        .parse()?;
    let directory = runtime.join("templaros-garden");
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!("journal directory must be owned and private");
    }
    Ok(directory.join(format!("shell-{pid}-{start}")))
}

#[derive(serde::Deserialize)]
struct ClaudeHookInput {
    hook_event_name: String,
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    notification_type: Option<String>,
}

/// Project only documented lifecycle fields; prompt, notification message, transcript_path,
/// tool input, argv, and any other fields are never forwarded to sessiond.
pub fn claude_observation(input: impl Read) -> Result<Option<String>> {
    // Deserialize unknown fields as IgnoredAny, not Value: raw prompts, notification
    // messages and Stop response text are never allocated or retained by this adapter.
    let json: ClaudeHookInput = serde_json::from_reader(input.take(1_048_576))?;
    let event = match json.hook_event_name.as_str() {
        "SessionStart" => "session-start",
        "UserPromptSubmit" => "turn-start",
        "Stop" => "stop",
        "SessionEnd" => "session-end",
        "Notification" => match json.notification_type.as_deref() {
            Some("permission_prompt" | "idle_prompt" | "elicitation_dialog") => "notification",
            // Authentication and informational notifications don't mean waiting for the user.
            _ => return Ok(None),
        },
        _ => bail!("unsupported Claude hook event"),
    };
    let id = json
        .session_id
        .as_deref()
        .context("missing agent hook session_id")?;
    if !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid native session identifier");
    }
    let cwd = json.cwd.as_ref().context("missing agent hook cwd")?;
    if !cwd.is_absolute() {
        bail!("agent cwd must be absolute");
    }
    Ok(Some(serde_json::json!({"schema_version":1,"adapter":"claude-code","event":event,"session_id":id,"cwd":cwd}).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_keeps_only_public_lifecycle_metadata() {
        let input = br#"{"hook_event_name":"Notification","notification_type":"permission_prompt","session_id":"test-123","cwd":"/tmp","message":"SECRET","prompt":"SECRET","transcript_path":"/secret"}"#;
        let output = claude_observation(&input[..]).unwrap().unwrap();
        assert!(!output.contains("SECRET"));
        assert!(!output.contains("transcript"));
        assert!(output.contains("notification"));
        assert!(
            claude_observation(
                &br#"{"hook_event_name":"Notification","notification_type":"auth_success"}"#[..]
            )
            .unwrap()
            .is_none()
        );
        assert!(
            claude_observation(
                &br#"{"hook_event_name":"Stop","session_id":"--evil","cwd":"/tmp"}"#[..]
            )
            .is_err()
        );
    }
    #[test]
    fn bash_journal_preserves_pipeline_status_user_hooks_and_ignorespace_privacy() {
        use std::{
            io::Write,
            os::unix::fs::PermissionsExt,
            process::{Command, Stdio},
        };
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(runtime.join("templaros-garden")).unwrap();
        let binary = temp.path().join("templar");
        fs::write(&binary, "#!/bin/sh\ncase \"$1:$2\" in garden:journal-init) printf '%s/templaros-garden/shell-test\\n' \"$XDG_RUNTIME_DIR\";; *) printf '%s\\n' \"$*\" >> \"$TRACE_FILE\";; esac\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../session/profile.d");
        let rc = temp.path().join("rc");
        fs::write(
            &rc,
            format!(
                "HISTCONTROL=ignorespace\nHISTFILE=/dev/null\nTEMPLAR_NOTIFY_MIN_SECONDS=999999\n\
             __user_prompt() {{ printf 'prior:%s\\n' \"$?\" >> \"$TRACE_FILE\"; }}\n\
             PROMPT_COMMAND=(__user_prompt)\nsource '{}'\nsource '{}'\n",
                root.join("60-templaros-garden.sh").display(),
                root.join("61-templaros-completion.sh").display(),
            ),
        )
        .unwrap();
        // --rcfile still reads Debian's system bash.bashrc. Installed hooks can satisfy
        // our idempotence guards before the source under test loads. Start without any
        // rc files or inherited shell integration, then source only this owned fixture.
        let mut child = Command::new("bash")
            .args(["--noprofile", "--norc", "-i"])
            .env_clear()
            .current_dir(temp.path())
            .env("HOME", temp.path())
            .env("TERM", "dumb")
            .env("TEMPLAR_TEST_RC", &rc)
            .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
            .env("XDG_CURRENT_DESKTOP", "TemplarOS")
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("TRACE_FILE", temp.path().join("trace"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(
            b"source \"$TEMPLAR_TEST_RC\"\nset -o pipefail\nfalse | true\nexport TEMPLAR_GARDEN_FULL_COMMANDS=1\nprintf 'VISIBLE_VALUE\\n'\n printf 'PRIVATE_VALUE\\n'\nfalse\nexit 0\n",
        ).unwrap();
        let output = child.wait_with_output().unwrap();
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{diagnostic}");
        let mut records = Vec::new();
        for slot in 0..32 {
            if let Ok(record) =
                fs::read_to_string(runtime.join(format!("templaros-garden/shell-test.{slot}")))
            {
                assert!(!record.contains("PRIVATE_VALUE"), "{record}");
                records.push(record);
            }
        }
        assert!(
            records
                .iter()
                .any(|record| record.contains("VISIBLE_VALUE")),
            "{records:?}\n{diagnostic}"
        );
        assert!(
            records.iter().any(|record| {
                let fields: Vec<_> = record.trim_end_matches('\n').split('\t').collect();
                fields.len() == 9 && fields[4] == "1" && fields[5] == "1 0"
            }),
            "{records:?}"
        );
        let trace = fs::read_to_string(temp.path().join("trace")).unwrap();
        assert!(trace.contains("prior:1"), "{trace}");
        assert!(trace.contains("garden observe terminal-entered"), "{trace}");
    }
}
