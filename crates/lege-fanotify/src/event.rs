//! Parsing what the kernel wrote, in safe code.
//!
//! A fanotify read returns a packed stream of `struct fanotify_event_metadata`, each followed by
//! zero or more variable-length *info records*. In the `FAN_REPORT_DFID_NAME` modes the record
//! that matters carries a filesystem id, an opaque file handle for a **directory**, and the name
//! of the entry inside it — never a path, and never a file descriptor.
//!
//! The whole parser works on `&[u8]` with explicit bounds checks. A short, truncated or
//! nonsensical record ends the parse rather than reading past the end: the kernel should never
//! produce one, and a backend that trusts a length field it did not check is exactly the class of
//! bug that makes people avoid raw bindings in the first place.

/// A directory, as fanotify names it.
///
/// The key of the map that gives this crate its per-directory attribution. `fsid` is included
/// because a file handle is only unique within one filesystem — and on Btrfs, within one
/// subvolume.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DirKey {
    pub fsid: [i32; 2],
    pub handle_type: i32,
    pub handle: Vec<u8>,
}

/// One info record, already decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FidRecord {
    /// `FAN_EVENT_INFO_TYPE_*`.
    pub info_type: u8,
    pub key: DirKey,
    /// The entry's name within `key`, when the record type carries one. Never valid UTF-8 by
    /// assumption: it is bytes, like every other path component in this codebase.
    pub name: Option<Vec<u8>>,
}

/// One event as it left the kernel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub mask: u64,
    pub records: Vec<FidRecord>,
    /// Set when the kernel handed us a descriptor anyway. In every mode this crate uses it is
    /// `FAN_NOFD`; carried so a caller can close it rather than leak it if that ever changes.
    pub fd: i32,
}

impl Event {
    /// The first record of one of the given types.
    #[must_use]
    pub fn record(&self, info_type: u8) -> Option<&FidRecord> {
        self.records
            .iter()
            .find(|record| record.info_type == info_type)
    }

    #[must_use]
    pub fn is_overflow(&self) -> bool {
        self.mask & libc::FAN_Q_OVERFLOW != 0
    }
}

/// `struct fanotify_event_metadata` is 24 bytes on every architecture with an 8-byte `__u64`.
const METADATA_LEN: usize = 24;
/// `struct fanotify_event_info_header`.
const INFO_HEADER_LEN: usize = 4;
/// `__kernel_fsid_t` plus `struct file_handle`'s two fixed fields.
const FID_PREFIX_LEN: usize = 8 + 4 + 4;

/// Why a buffer could not be read as a sequence of events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The kernel's ABI version is not the one this parser was written against.
    Version(u8),
    /// A length field ran past the end of the buffer, or was too small to be a record.
    Malformed(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(version) => write!(
                formatter,
                "fanotify reported metadata version {version}, expected {}",
                libc::FANOTIFY_METADATA_VERSION
            ),
            Self::Malformed(what) => write!(formatter, "malformed fanotify event: {what}"),
        }
    }
}

impl std::error::Error for ParseError {}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|slice| u16::from_ne_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|slice| u32::from_ne_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes.get(offset..offset + 8).map(|slice| {
        u64::from_ne_bytes([
            slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
        ])
    })
}

/// Decode every event in one `read(2)` worth of bytes.
pub fn parse(buffer: &[u8]) -> Result<Vec<Event>, ParseError> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    while offset + METADATA_LEN <= buffer.len() {
        let event_len =
            u32_at(buffer, offset).ok_or(ParseError::Malformed("truncated event_len"))? as usize;
        if event_len < METADATA_LEN || offset + event_len > buffer.len() {
            return Err(ParseError::Malformed("event_len outside the buffer"));
        }
        let version = buffer[offset + 4];
        if version != libc::FANOTIFY_METADATA_VERSION {
            return Err(ParseError::Version(version));
        }
        let metadata_len = u16_at(buffer, offset + 6)
            .ok_or(ParseError::Malformed("truncated metadata_len"))?
            as usize;
        if metadata_len < METADATA_LEN || metadata_len > event_len {
            return Err(ParseError::Malformed("metadata_len outside the event"));
        }
        let mask = u64_at(buffer, offset + 8).ok_or(ParseError::Malformed("truncated mask"))?;
        let fd = u32_at(buffer, offset + 16).ok_or(ParseError::Malformed("truncated fd"))? as i32;

        let records = parse_records(&buffer[offset + metadata_len..offset + event_len])?;
        events.push(Event { mask, records, fd });
        offset += event_len;
    }
    Ok(events)
}

fn parse_records(mut tail: &[u8]) -> Result<Vec<FidRecord>, ParseError> {
    let mut records = Vec::new();
    while tail.len() >= INFO_HEADER_LEN {
        let info_type = tail[0];
        let len = u16_at(tail, 2).ok_or(ParseError::Malformed("truncated info header"))? as usize;
        if len < INFO_HEADER_LEN || len > tail.len() {
            return Err(ParseError::Malformed(
                "info record length outside the event",
            ));
        }
        if let Some(record) = parse_fid(info_type, &tail[..len]) {
            records.push(record);
        }
        tail = &tail[len..];
    }
    Ok(records)
}

/// The fid-bearing record types. Anything else (a pidfd, an error record) is skipped: this crate
/// never asks for them, and an unrecognised record must not abort a batch of good events.
fn parse_fid(info_type: u8, record: &[u8]) -> Option<FidRecord> {
    let named = matches!(
        info_type,
        libc::FAN_EVENT_INFO_TYPE_DFID_NAME
            | libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME
            | libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME
    );
    if !named
        && !matches!(
            info_type,
            libc::FAN_EVENT_INFO_TYPE_FID | libc::FAN_EVENT_INFO_TYPE_DFID
        )
    {
        return None;
    }
    let body = record.get(INFO_HEADER_LEN..)?;
    if body.len() < FID_PREFIX_LEN {
        return None;
    }
    let fsid = [u32_at(body, 0)? as i32, u32_at(body, 4)? as i32];
    let handle_bytes = u32_at(body, 8)? as usize;
    let handle_type = u32_at(body, 12)? as i32;
    let handle = body
        .get(FID_PREFIX_LEN..FID_PREFIX_LEN + handle_bytes)?
        .to_vec();
    let name = if named {
        let rest = body.get(FID_PREFIX_LEN + handle_bytes..)?;
        // The name is NUL-terminated and the record is padded to an 8-byte boundary after it, so
        // read up to the first NUL rather than to the end of the record.
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(rest.len());
        Some(rest[..end].to_vec())
    } else {
        None
    };
    Some(FidRecord {
        info_type,
        key: DirKey {
            fsid,
            handle_type,
            handle,
        },
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::{DirKey, Event, FidRecord, METADATA_LEN, ParseError, parse};

    /// `(info_type, fsid, handle_type, handle, name)` — one info record, as the kernel writes it.
    type Record<'a> = (u8, [i32; 2], i32, &'a [u8], Option<&'a [u8]>);

    /// Build one event the way the kernel lays it out, so the parser is tested against the ABI
    /// rather than against itself.
    fn encode(mask: u64, records: &[Record<'_>]) -> Vec<u8> {
        let mut tail = Vec::new();
        for (info_type, fsid, handle_type, handle, name) in records {
            let mut body = Vec::new();
            body.extend_from_slice(&fsid[0].to_ne_bytes());
            body.extend_from_slice(&fsid[1].to_ne_bytes());
            body.extend_from_slice(&(handle.len() as u32).to_ne_bytes());
            body.extend_from_slice(&handle_type.to_ne_bytes());
            body.extend_from_slice(handle);
            if let Some(name) = name {
                body.extend_from_slice(name);
                body.push(0);
            }
            while (4 + body.len()) % 8 != 0 {
                body.push(0);
            }
            let len = u16::try_from(4 + body.len()).unwrap();
            tail.push(*info_type);
            tail.push(0);
            tail.extend_from_slice(&len.to_ne_bytes());
            tail.extend_from_slice(&body);
        }
        let event_len = u32::try_from(METADATA_LEN + tail.len()).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&event_len.to_ne_bytes());
        out.push(libc::FANOTIFY_METADATA_VERSION);
        out.push(0);
        out.extend_from_slice(&u16::try_from(METADATA_LEN).unwrap().to_ne_bytes());
        out.extend_from_slice(&mask.to_ne_bytes());
        out.extend_from_slice(&(-1i32).to_ne_bytes());
        out.extend_from_slice(&0i32.to_ne_bytes());
        out.extend_from_slice(&tail);
        out
    }

    #[test]
    fn a_named_directory_record_round_trips() {
        let bytes = encode(
            libc::FAN_CREATE,
            &[(
                libc::FAN_EVENT_INFO_TYPE_DFID_NAME,
                [7, 9],
                1,
                &[1, 2, 3, 4, 5, 6, 7, 8],
                Some(b"cargo.toml"),
            )],
        );
        let events = parse(&bytes).expect("a well-formed event parses");
        assert_eq!(
            events,
            vec![Event {
                mask: libc::FAN_CREATE,
                fd: -1,
                records: vec![FidRecord {
                    info_type: libc::FAN_EVENT_INFO_TYPE_DFID_NAME,
                    key: DirKey {
                        fsid: [7, 9],
                        handle_type: 1,
                        handle: vec![1, 2, 3, 4, 5, 6, 7, 8],
                    },
                    name: Some(b"cargo.toml".to_vec()),
                }],
            }]
        );
    }

    /// A rename arrives as one event carrying both halves. This is the shape the whole backend
    /// depends on: inotify pairs two events by cookie, fanotify hands over the pair already made.
    #[test]
    fn a_rename_carries_both_directories_in_one_event() {
        let bytes = encode(
            libc::FAN_RENAME,
            &[
                (
                    libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME,
                    [7, 9],
                    1,
                    &[1, 1, 1, 1],
                    Some(b"before"),
                ),
                (
                    libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME,
                    [7, 9],
                    1,
                    &[2, 2, 2, 2],
                    Some(b"after"),
                ),
            ],
        );
        let events = parse(&bytes).expect("a rename parses");
        let event = &events[0];
        assert_eq!(event.records.len(), 2);
        assert_eq!(
            event
                .record(libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME)
                .and_then(|record| record.name.clone()),
            Some(b"before".to_vec())
        );
        assert_eq!(
            event
                .record(libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME)
                .and_then(|record| record.name.clone()),
            Some(b"after".to_vec())
        );
    }

    #[test]
    fn two_events_in_one_read_both_come_back() {
        let mut bytes = encode(libc::FAN_CREATE, &[]);
        bytes.extend(encode(libc::FAN_DELETE, &[]));
        let events = parse(&bytes).expect("a packed pair parses");
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].mask, libc::FAN_DELETE);
    }

    /// The bounds checks are the point of writing the parser in safe code, so they are tested.
    #[test]
    fn a_length_past_the_end_is_refused_rather_than_read() {
        let mut bytes = encode(libc::FAN_CREATE, &[]);
        bytes[0] = 0xff;
        assert_eq!(
            parse(&bytes),
            Err(ParseError::Malformed("event_len outside the buffer"))
        );
    }

    #[test]
    fn an_unknown_abi_version_is_refused_rather_than_guessed() {
        let mut bytes = encode(libc::FAN_CREATE, &[]);
        bytes[4] = libc::FANOTIFY_METADATA_VERSION + 1;
        assert_eq!(
            parse(&bytes),
            Err(ParseError::Version(libc::FANOTIFY_METADATA_VERSION + 1))
        );
    }

    #[test]
    fn an_unrecognised_info_record_is_skipped_not_fatal() {
        let bytes = encode(
            libc::FAN_CREATE,
            &[(libc::FAN_EVENT_INFO_TYPE_PIDFD, [0, 0], 0, &[], None)],
        );
        let events = parse(&bytes).expect("an unknown record type does not abort the batch");
        assert!(events[0].records.is_empty());
    }
}
