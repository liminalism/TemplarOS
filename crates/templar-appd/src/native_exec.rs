//! Lossless transport for an already-brokered Unix executable, argv and working directory.
//!
//! The systemd launch command invokes [`ADAPTER_PATH`] with one typed JSON argument;
//! the adapter decodes native bytes, establishes cwd and replaces itself with the application.
//! Neither desktop field codes, shell syntax nor environment substitutions are interpreted here.
//! Payloads contain application arguments and can contain secrets: errors must never
//! include the payload or argument contents.

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeSeq};
use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

/// Installed native argv/cwd adapter, invoked directly by systemd rather than a shell.
pub const ADAPTER_PATH: &str = "/usr/libexec/templaros/templar-app-exec";

/// Leave room for the terminating NUL in Linux's 128 KiB single-argument limit.
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024 - 1;
/// Bound argument-vector overhead as well as the serialized payload size.
pub const MAX_ARGUMENTS: usize = 4096;

#[derive(Serialize)]
struct EncodedPayload<'a> {
    #[serde(serialize_with = "templar_types::native_path::serialize")]
    executable: &'a Path,
    arguments: NativeArguments<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<NativePath<'a>>,
}

struct NativePath<'a>(&'a Path);

impl Serialize for NativePath<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        templar_types::native_path::serialize(self.0, serializer)
    }
}

struct NativeArguments<'a>(&'a [OsString]);

impl Serialize for NativeArguments<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for argument in self.0 {
            sequence.serialize_element(argument.as_os_str().as_bytes())?;
        }
        sequence.end()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedPayload {
    #[serde(with = "templar_types::native_path")]
    executable: PathBuf,
    arguments: Vec<Vec<u8>>,
    #[serde(default, with = "templar_types::native_path::optional")]
    directory: Option<PathBuf>,
}

/// Decoded transport, intentionally without Debug: native paths/argv may contain secrets.
pub struct NativeLaunch {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub directory: Option<PathBuf>,
}

impl std::fmt::Debug for NativeLaunch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NativeLaunch { <redacted> }")
    }
}

/// Encode native bytes without UTF-8 conversion or shell/systemd escaping.
///
/// The executable and optional cwd must be absolute, every string must be NUL-free,
/// and the result must fit [`MAX_PAYLOAD_BYTES`] with at most [`MAX_ARGUMENTS`] arguments.
pub fn encode(
    executable: &Path,
    arguments: &[OsString],
    directory: Option<&Path>,
) -> Result<String> {
    ensure!(
        executable.is_absolute(),
        "native executable must be absolute"
    );
    if let Some(directory) = directory {
        ensure!(directory.is_absolute(), "native directory must be absolute");
    }
    ensure!(
        arguments.len() <= MAX_ARGUMENTS,
        "too many native arguments"
    );

    // Compute the exact compact JSON size before allocating its output. Serialization
    // borrows native bytes; it does not copy them into an intermediate byte vector.
    let mut size = r#"{"executable":,"arguments":[]}"#.len();
    add_path_size(&mut size, executable).context("invalid native executable")?;
    if let Some(directory) = directory {
        size += r#","directory":"#.len();
        add_path_size(&mut size, directory).context("invalid native directory")?;
    }
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            size += 1;
        }
        add_byte_array_size(&mut size, argument.as_os_str().as_bytes())
            .with_context(|| format!("invalid native argument at index {index}"))?;
    }
    ensure!(
        size <= MAX_PAYLOAD_BYTES,
        "native payload exceeds byte limit"
    );

    let mut payload = Vec::with_capacity(size);
    serde_json::to_writer(
        &mut payload,
        &EncodedPayload {
            executable,
            arguments: NativeArguments(arguments),
            directory: directory.map(NativePath),
        },
    )
    .context("encoding native launch payload")?;
    String::from_utf8(payload).context("encoded native launch payload is not UTF-8")
}

fn add_byte_array_size(size: &mut usize, bytes: &[u8]) -> Result<()> {
    // Each byte contributes at least one digit. Reject huge inputs before traversing
    // them to compute their decimal representation's exact size.
    ensure!(
        bytes.len() <= MAX_PAYLOAD_BYTES,
        "native payload exceeds byte limit"
    );
    *size += 2;
    for (index, byte) in bytes.iter().enumerate() {
        ensure!(*byte != 0, "native strings must not contain NUL");
        *size += usize::from(index != 0)
            + if *byte >= 100 {
                3
            } else if *byte >= 10 {
                2
            } else {
                1
            };
        ensure!(
            *size <= MAX_PAYLOAD_BYTES,
            "native payload exceeds byte limit"
        );
    }
    Ok(())
}

fn add_path_size(size: &mut usize, path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        bytes.len() <= MAX_PAYLOAD_BYTES,
        "native payload exceeds byte limit"
    );
    ensure!(!bytes.contains(&0), "native paths must not contain NUL");
    // Exact compact-JSON size of the shared native_path codec, without allocating
    // an intermediate JSON string or an additional base64 path.
    *size += if path.to_str().is_some() {
        2 + bytes
            .iter()
            .map(|byte| match byte {
                b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 2,
                0..=31 => 6,
                _ => 1,
            })
            .sum::<usize>()
    } else {
        r#"{"path_b64":""}"#.len() + bytes.len().div_ceil(3) * 4
    };
    ensure!(
        *size <= MAX_PAYLOAD_BYTES,
        "native payload exceeds byte limit"
    );
    Ok(())
}

/// Decode a bounded JSON payload into the exact native executable, argv and cwd bytes.
///
/// This only validates the transport: authorization, directory existence and executable
/// selection belong to the broker. The adapter preserves its inherited environment.
pub fn decode(payload: &str) -> Result<NativeLaunch> {
    ensure!(
        payload.len() <= MAX_PAYLOAD_BYTES,
        "native payload exceeds byte limit"
    );
    let decoded: DecodedPayload = serde_json::from_str(payload).map_err(|error| {
        // serde's messages can quote attacker-controlled field names/string values.
        // Retain a useful location without leaking native argv or payload contents.
        anyhow!(
            "invalid native launch payload at line {} column {}",
            error.line(),
            error.column()
        )
    })?;
    ensure!(
        decoded.arguments.len() <= MAX_ARGUMENTS,
        "too many native arguments"
    );
    ensure!(
        !decoded.executable.as_os_str().as_bytes().contains(&0),
        "native executable contains NUL"
    );
    for (index, argument) in decoded.arguments.iter().enumerate() {
        ensure!(
            !argument.contains(&0),
            "native argument at index {index} contains NUL"
        );
    }
    ensure!(
        decoded.executable.is_absolute(),
        "native executable must be absolute"
    );
    if let Some(directory) = &decoded.directory {
        ensure!(directory.is_absolute(), "native directory must be absolute");
    }
    let arguments = decoded
        .arguments
        .into_iter()
        .map(OsString::from_vec)
        .collect();
    Ok(NativeLaunch {
        executable: decoded.executable,
        arguments,
        directory: decoded.directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_distinct_non_utf8_native_bytes() {
        let executable = PathBuf::from(OsString::from_vec(b"/opt/viewer-\xff".to_vec()));
        let arguments = vec![
            OsString::from_vec(b"/tmp/name-\xff".to_vec()),
            OsString::from_vec(b"/tmp/name-\xfe".to_vec()),
            OsString::from_vec(vec![0x80, 0xc0, 0xed, 0xff]),
        ];
        let directory = PathBuf::from(OsString::from_vec(b"/tmp/project-\xfe".to_vec()));
        let payload = encode(&executable, &arguments, Some(&directory)).unwrap();
        assert!(payload.is_ascii());
        let actual = decode(&payload).unwrap();
        assert_eq!(actual.executable, executable);
        assert_eq!(actual.arguments, arguments);
        assert_eq!(actual.directory.as_deref(), Some(directory.as_path()));
        assert_ne!(actual.arguments[0], actual.arguments[1]);
        assert!(!format!("{actual:?}").contains("project"));
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(json["executable"]["path_b64"].is_string());
        assert!(json["directory"]["path_b64"].is_string());
    }

    #[test]
    fn whitespace_percent_shell_syntax_and_secrets_are_literal() {
        let arguments: Vec<OsString> = [
            "",
            "two words",
            " tab\tand\nnewline ",
            "%f %U %% %n",
            "$HOME ${TOKEN} $$ $(touch /tmp/not-executed) `id` ; & | *",
            "--secret=literal-token-123",
            "quotes: '\" \\ end",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let executable = Path::new("/usr/bin/viewer");
        let directory = Path::new("/tmp/é \"quotes\" \\ tab\tnewline\ncontrol\x01");
        let payload = encode(executable, &arguments, Some(directory)).unwrap();
        let decoded = decode(&payload).unwrap();
        assert_eq!(decoded.executable, executable);
        assert_eq!(decoded.arguments, arguments);
        assert_eq!(decoded.directory.as_deref(), Some(directory));
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["executable"], "/usr/bin/viewer");
        assert_eq!(json["directory"], directory.to_str().unwrap());
    }

    #[test]
    fn empty_argv_is_valid() {
        let executable = Path::new("/usr/bin/viewer");
        let decoded = decode(&encode(executable, &[], None).unwrap()).unwrap();
        assert_eq!(decoded.executable, executable);
        assert!(decoded.arguments.is_empty());
        assert!(decoded.directory.is_none());
    }

    #[test]
    fn rejects_nul_and_non_absolute_executables_on_both_sides() {
        for executable in ["", "viewer", "./viewer", "/usr/bin/viewer\0suffix"] {
            assert!(encode(Path::new(executable), &[], None).is_err());
            let payload = serde_json::json!({
                "executable": executable, "arguments": []
            })
            .to_string();
            assert!(decode(&payload).is_err());
        }
        assert!(
            encode(
                Path::new("/usr/bin/viewer"),
                &[OsString::from("before\0after")],
                None
            )
            .is_err()
        );
        assert!(decode(r#"{"executable":"/x","arguments":[[65,0,66]]}"#).is_err());
        for directory in ["", "relative", "./relative", "/tmp/before\0after"] {
            assert!(encode(Path::new("/x"), &[], Some(Path::new(directory))).is_err());
            let payload = serde_json::json!({
                "executable": "/x", "arguments": [], "directory": directory
            })
            .to_string();
            assert!(decode(&payload).is_err());
        }
    }

    #[test]
    fn rejects_malformed_payloads_without_echoing_secrets() {
        for payload in [
            "",
            "null",
            "{}",
            "secret-token-not-json",
            r#"{"executable":{"path_b64":"secret-token"},"arguments":[]}"#,
            r#"{"executable":[47,256],"arguments":[]}"#,
            r#"{"executable":[47,-1],"arguments":[]}"#,
            r#"{"executable":"/x","arguments":[null]}"#,
            r#"{"executable":"/x","arguments":[],"extra":"secret-token"}"#,
            r#"{"executable":"/x","arguments":[],"secret-token-field":[]}"#,
            r#"{"executable":"/x","arguments":[["secret-token-value"]]}"#,
            r#"{"executable":"/x","arguments":[]} trailing-secret"#,
            r#"{"executable":"/x","arguments":[],"directory":{"path_b64":"secret-token"}}"#,
        ] {
            let error = decode(payload).unwrap_err();
            assert!(!format!("{error:#}").contains("secret-token"));
        }
    }

    #[test]
    fn rejects_excessive_argument_counts_on_both_sides() {
        let arguments = vec![OsString::new(); MAX_ARGUMENTS + 1];
        assert!(encode(Path::new("/x"), &arguments, None).is_err());
        let payload = serde_json::json!({
            "executable": "/x", "arguments": vec![Vec::<u8>::new(); MAX_ARGUMENTS + 1]
        })
        .to_string();
        assert!(decode(&payload).is_err());
        let payload = encode(Path::new("/x"), &arguments[..MAX_ARGUMENTS], None).unwrap();
        assert_eq!(decode(&payload).unwrap().arguments.len(), MAX_ARGUMENTS);
    }

    #[test]
    fn encoded_size_boundary_matches_decoder_limit() {
        assert_eq!(MAX_PAYLOAD_BYTES, 131071);
        let native_directory = PathBuf::from(OsString::from_vec(b"/tmp/project-\xff".to_vec()));
        for directory in [
            None,
            Some(Path::new("/tmp/é\n\"quoted\"\x01")),
            Some(native_directory.as_path()),
        ] {
            let empty_size = encode(Path::new("/x"), &[OsString::new()], directory)
                .unwrap()
                .len();
            // Byte 49 contributes three JSON bytes except for the first comma.
            // Replacing up to two bytes with 100 adds exactly the remaining bytes.
            let count = (MAX_PAYLOAD_BYTES - empty_size + 1) / 3;
            let mut bytes = vec![b'1'; count];
            let remainder = MAX_PAYLOAD_BYTES - (empty_size + count * 3 - 1);
            bytes[..remainder].fill(100);
            let argument = OsString::from_vec(bytes.clone());
            let payload = encode(Path::new("/x"), &[argument.clone()], directory).unwrap();
            assert_eq!(payload.len(), MAX_PAYLOAD_BYTES);
            assert_eq!(decode(&payload).unwrap().arguments, vec![argument]);
            bytes.push(b'1');
            assert!(encode(Path::new("/x"), &[OsString::from_vec(bytes)], directory).is_err());
            assert!(decode(&(payload + " ")).is_err());
        }
    }
}
