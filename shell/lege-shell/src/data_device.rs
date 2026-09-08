//! `wl_data_device`: carrying Home's clipboard and drags to other applications.
//!
//! [`crate::clipboard`] decides *what* a copy means and encodes it; this module is the transport
//! that hands those bytes to the rest of the desktop. The split is deliberate — the encodings can
//! be got exactly right and tested exhaustively with no compositor at all, and everything that
//! genuinely needs a live seat is confined here.
//!
//! # What the compositor already provides
//!
//! Nothing was needed on the `lege-comp` side. It implements `DataDeviceHandler`,
//! `SelectionHandler`, `WaylandDndGrabHandler` and `DndGrabHandler`, constructs a
//! `DataDeviceState`, and `smithay::delegate_dispatch2!` wires every one of them — so
//! `wl_data_device_manager` has been advertised all along and the shell simply never bound it.
//!
//! # The one rule that decides when a copy lands
//!
//! Smithay ignores the serial passed to `set_selection` and instead requires that the *client*
//! owning the data device holds keyboard focus (`selection/data_device/device.rs`: "denying
//! setting selection by a non-focused client"). The shell shares winit's connection, so "the
//! shell's client" and "the Home window" are the same client, and the compositor focuses the Home
//! surface on reveal and while no client window is visible
//! (`compositor/lege-comp/src/input.rs::refocus_after_reveal`, `focus_home_if_needed`). A person
//! pressing Ctrl+C in Home therefore has focus by construction.
//!
//! A serial is tracked and sent anyway, from this connection's own `wl_keyboard`/`wl_pointer`.
//! Smithay not checking it is Smithay's choice, not a licence to send a made-up number: the
//! protocol asks for the serial of the input event that caused the request, other compositors do
//! check, and a shell that sends zero is a shell that breaks the day it meets one.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::clipboard::{Clipboard, ClipboardMode};

/// The interchange type every desktop reads.
pub const URI_LIST: &str = lege_types::uri_list::MIME_TYPE;
/// The de-facto type that carries a cut/copy distinction.
pub const GNOME_COPIED_FILES: &str = crate::clipboard::GNOME_COPIED_FILES;

/// How long to wait for another application to write its side of a selection transfer.
///
/// Bounded because this runs on the frame thread: an application that offers a target and then
/// never writes it must cost one slow paste, not a frozen desktop.
const TRANSFER_TIMEOUT: Duration = Duration::from_millis(250);

/// The most a selection transfer will read. A clipboard full of file paths is kilobytes; anything
/// past this is a different kind of payload arriving under a uri-list's name.
const TRANSFER_LIMIT: usize = 4 * 1024 * 1024;

/// The bytes to serve for each offered MIME type, prepared before the selection is set.
///
/// Prepared up front rather than rendered on demand: a `send` request arrives on the Wayland
/// queue, possibly long after the copy, and the selection must be whatever was copied *then* even
/// if the person has since changed what is selected in Home.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardPayload {
    pub uri_list: String,
    pub gnome_copied_files: String,
}

impl ClipboardPayload {
    pub fn of(clipboard: &Clipboard) -> ClipboardPayload {
        ClipboardPayload {
            uri_list: clipboard.to_uri_list(),
            gnome_copied_files: clipboard.to_gnome_copied_files(),
        }
    }

    /// The MIME types to offer, in the order they are advertised.
    ///
    /// The GNOME target first because a receiver that understands both should prefer the one that
    /// carries the mode; `text/uri-list` second so that everything else still gets the files.
    pub fn offered_mime_types() -> [&'static str; 2] {
        [GNOME_COPIED_FILES, URI_LIST]
    }

    /// What to write when a receiver asks for `mime_type`, or `None` for one never offered.
    pub fn bytes_for(&self, mime_type: &str) -> Option<&[u8]> {
        match mime_type {
            URI_LIST => Some(self.uri_list.as_bytes()),
            GNOME_COPIED_FILES => Some(self.gnome_copied_files.as_bytes()),
            _ => None,
        }
    }
}

/// The target to ask a foreign offer for, chosen from what it advertises.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignTarget {
    /// Carries the cut/copy mode as well as the paths.
    GnomeCopiedFiles,
    /// Paths only; read as a copy.
    UriList,
}

impl ForeignTarget {
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::GnomeCopiedFiles => GNOME_COPIED_FILES,
            Self::UriList => URI_LIST,
        }
    }
}

/// Pick the best target an offer advertises, or `None` when it holds no files.
///
/// The GNOME target wins whenever it is present, because it is the only one that distinguishes a
/// cut from a copy — asking for `text/uri-list` from an application that offered both would throw
/// away the very fact that makes a paste a move.
pub fn choose_target(mime_types: &[String]) -> Option<ForeignTarget> {
    let has = |wanted: &str| mime_types.iter().any(|offered| offered == wanted);
    if has(GNOME_COPIED_FILES) {
        Some(ForeignTarget::GnomeCopiedFiles)
    } else if has(URI_LIST) {
        Some(ForeignTarget::UriList)
    } else {
        None
    }
}

/// Turn a transferred payload into a clipboard.
pub fn clipboard_from_transfer(target: ForeignTarget, payload: &str) -> Option<Clipboard> {
    match target {
        ForeignTarget::GnomeCopiedFiles => Clipboard::from_foreign(Some(payload), None),
        ForeignTarget::UriList => Clipboard::from_foreign(None, Some(payload)),
    }
}

/// Where a paste should take its paths from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasteSource {
    /// Home's own clipboard: it is still what the seat is offering.
    Own,
    /// Somebody else owns the selection now; read theirs.
    Foreign,
    /// Nothing to paste from either side.
    Nothing,
}

/// Decide which clipboard a paste reads.
///
/// The rule is ownership, not recency, and it is the one the protocol already answers for us: a
/// `wl_data_source` is `cancelled` the moment another client takes the selection, so "is Home
/// still the owner" is a fact the compositor told us rather than a guess. Holding on to a stale
/// local clipboard after another application has copied something is the bug this prevents —
/// Ctrl+C in a browser then Ctrl+V in Home must paste the browser's files, not the ones Home
/// copied ten minutes ago.
pub fn paste_source(
    own_selection_live: bool,
    own: Option<&Clipboard>,
    foreign_offer: bool,
) -> PasteSource {
    match (own_selection_live && own.is_some(), foreign_offer) {
        (true, _) => PasteSource::Own,
        (false, true) => PasteSource::Foreign,
        (false, false) => {
            // Not the owner and nothing on offer. A local clipboard from before the shell ever
            // owned a selection — the compositor was absent, say — is still better than refusing.
            if own.is_some() {
                PasteSource::Own
            } else {
                PasteSource::Nothing
            }
        }
    }
}

/// The last input serial this connection saw, for requests that must name one.
///
/// Every event that carries a serial updates it. Which event it came from does not matter to the
/// protocol — what matters is that it is a serial the compositor issued to *this* client for a
/// real input event, rather than a number the shell invented.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputSerials {
    last: Option<u32>,
}

impl InputSerials {
    /// Note a serial from an input event. Serials only ever go forward, so an event dispatched out
    /// of order cannot walk the record backwards.
    pub fn record(&mut self, serial: u32) {
        if self.last.is_none_or(|last| serial > last) {
            self.last = Some(serial);
        }
    }

    /// The serial to send, or `None` when this client has seen no input at all.
    ///
    /// `None` rather than zero: a request that must name a real serial and has none should not be
    /// sent, and a zero would be a lie that a stricter compositor is right to reject.
    pub fn latest(&self) -> Option<u32> {
        self.last
    }
}

/// Read one selection transfer to its end, bounded in both time and size.
///
/// The write end has already been handed to the other application and dropped here, so the read
/// ends at their `close` — but an application that never writes must not be able to hang the
/// frame thread, hence the deadline. A short read is returned rather than discarded: a truncated
/// uri-list decodes to the paths that did arrive, which is a better answer than none.
pub fn read_transfer(read_end: OwnedFd) -> std::io::Result<String> {
    use std::os::fd::AsRawFd;
    // Non-blocking, so the deadline below is real rather than advisory.
    let raw = read_end.as_raw_fd();
    // SAFETY: `raw` is a live fd this function owns for its whole body; `fcntl` with F_SETFL only
    // changes that descriptor's status flags and cannot invalidate it.
    #[allow(unsafe_code)]
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let mut file = std::fs::File::from(read_end);
    let mut out = Vec::new();
    let mut buffer = [0u8; 8192];
    let deadline = Instant::now() + TRANSFER_TIMEOUT;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                out.extend_from_slice(&buffer[..read]);
                if out.len() >= TRANSFER_LIMIT {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                // Rounded up: a truncated millisecond would wake the poll just before the
                // deadline and read as an expiry that has not happened yet.
                let timeout_ms =
                    i32::try_from(remaining.as_millis().saturating_add(1)).unwrap_or(i32::MAX);
                let mut descriptor = libc::pollfd {
                    fd: raw,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: `descriptor` is one valid pollfd and `raw` remains owned by `file`.
                #[allow(unsafe_code)]
                let ready = unsafe { libc::poll(&raw mut descriptor, 1, timeout_ms) };
                if ready == 0 {
                    break;
                }
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// What a completed foreign read produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForeignSelection {
    pub paths: Vec<PathBuf>,
    pub mode: ClipboardMode,
}

impl ForeignSelection {
    pub fn of(clipboard: &Clipboard) -> ForeignSelection {
        ForeignSelection {
            paths: clipboard.paths().to_vec(),
            mode: clipboard.mode(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clipboard() -> Clipboard {
        Clipboard::copy(vec![
            PathBuf::from("/home/u/one.txt"),
            PathBuf::from("/home/u/two.txt"),
        ])
        .unwrap()
    }

    #[test]
    fn data_device_offers_both_targets_and_serves_exactly_what_was_copied() {
        let payload = ClipboardPayload::of(&clipboard());
        assert_eq!(
            ClipboardPayload::offered_mime_types(),
            [GNOME_COPIED_FILES, URI_LIST],
            "the target that carries the mode is advertised first"
        );
        assert_eq!(
            payload.bytes_for(URI_LIST),
            Some("file:///home/u/one.txt\r\nfile:///home/u/two.txt\r\n".as_bytes())
        );
        assert_eq!(
            payload.bytes_for(GNOME_COPIED_FILES),
            Some("copy\nfile:///home/u/one.txt\nfile:///home/u/two.txt".as_bytes())
        );
        // A receiver asking for something never offered gets nothing, not an empty file that
        // looks like an answer.
        assert_eq!(payload.bytes_for("text/plain"), None);
        assert_eq!(payload.bytes_for(""), None);
    }

    #[test]
    fn data_device_a_cut_is_served_as_a_cut() {
        let cut = Clipboard::cut(vec![PathBuf::from("/home/u/one.txt")]).unwrap();
        let payload = ClipboardPayload::of(&cut);
        assert!(
            payload.gnome_copied_files.starts_with("cut\n"),
            "{}",
            payload.gnome_copied_files
        );
        // The uri-list is identical either way — which is exactly why the other target exists.
        assert_eq!(
            payload.uri_list,
            ClipboardPayload::of(&Clipboard::copy(vec![PathBuf::from("/home/u/one.txt")]).unwrap())
                .uri_list
        );
    }

    #[test]
    fn data_device_asks_a_foreign_offer_for_the_target_that_keeps_the_most_meaning() {
        let both = vec![URI_LIST.to_owned(), GNOME_COPIED_FILES.to_owned()];
        assert_eq!(
            choose_target(&both),
            Some(ForeignTarget::GnomeCopiedFiles),
            "asking for the uri-list here would throw away the cut"
        );
        assert_eq!(
            choose_target(&[URI_LIST.to_owned()]),
            Some(ForeignTarget::UriList)
        );
        // An offer with no files is not something to paste.
        assert_eq!(choose_target(&["text/plain".to_owned()]), None);
        assert_eq!(choose_target(&[]), None);
    }

    #[test]
    fn data_device_reads_a_foreign_cut_as_a_move_and_a_bare_list_as_a_copy() {
        let cut = clipboard_from_transfer(
            ForeignTarget::GnomeCopiedFiles,
            "cut\nfile:///home/u/moved.txt",
        )
        .unwrap();
        assert_eq!(cut.mode(), ClipboardMode::Cut);
        assert_eq!(cut.paths(), [PathBuf::from("/home/u/moved.txt")]);

        let copied =
            clipboard_from_transfer(ForeignTarget::UriList, "file:///home/u/copied.txt\r\n")
                .unwrap();
        assert_eq!(copied.mode(), ClipboardMode::Copy);
        // A transfer that arrives empty or unreadable is not a clipboard.
        assert_eq!(clipboard_from_transfer(ForeignTarget::UriList, ""), None);
        assert_eq!(
            clipboard_from_transfer(ForeignTarget::GnomeCopiedFiles, "nonsense"),
            None
        );
    }

    #[test]
    fn data_device_paste_prefers_whichever_side_actually_owns_the_selection() {
        let own = clipboard();
        // Home copied last and still owns the selection.
        assert_eq!(
            paste_source(true, Some(&own), true),
            PasteSource::Own,
            "owning the selection means Home's own copy is the live one"
        );
        // Another application copied since, so Home's source was cancelled: theirs wins, even
        // though Home still remembers what it copied.
        assert_eq!(paste_source(false, Some(&own), true), PasteSource::Foreign);
        // Nobody is offering anything readable, but Home has a clipboard from before it ever
        // owned a selection — better than refusing to paste.
        assert_eq!(paste_source(false, Some(&own), false), PasteSource::Own);
        assert_eq!(paste_source(false, None, true), PasteSource::Foreign);
        assert_eq!(paste_source(false, None, false), PasteSource::Nothing);
        assert_eq!(paste_source(true, None, true), PasteSource::Foreign);
    }

    #[test]
    fn data_device_serials_only_move_forward_and_never_invent_one() {
        let mut serials = InputSerials::default();
        // Nothing seen yet: no serial to send, and zero is not an acceptable substitute.
        assert_eq!(serials.latest(), None);
        serials.record(41);
        serials.record(42);
        assert_eq!(serials.latest(), Some(42));
        // An event dispatched out of order must not walk the record backwards.
        serials.record(7);
        assert_eq!(serials.latest(), Some(42));
    }

    #[test]
    fn data_device_reads_a_transfer_to_its_end_and_stops_at_the_writer_s_close() {
        let (read_end, write_end) = std::io::pipe().unwrap();
        let payload = "file:///home/u/one.txt\r\nfile:///home/u/two.txt\r\n";
        std::thread::spawn(move || {
            use std::io::Write as _;
            let mut write_end = write_end;
            // Written in two goes, as a real sender's chunks arrive.
            write_end.write_all(&payload.as_bytes()[..12]).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            write_end.write_all(&payload.as_bytes()[12..]).unwrap();
            // Dropping the write end is what ends the read.
        });
        assert_eq!(read_transfer(OwnedFd::from(read_end)).unwrap(), payload);
    }

    #[test]
    fn data_device_a_sender_that_never_writes_costs_one_slow_paste_not_the_desktop() {
        let (read_end, write_end) = std::io::pipe().unwrap();
        // Held open and never written to: the read has to give up on its own.
        let started = Instant::now();
        let transferred = read_transfer(OwnedFd::from(read_end)).unwrap();
        drop(write_end);
        assert_eq!(transferred, "");
        assert!(
            started.elapsed() >= TRANSFER_TIMEOUT,
            "it gave up before the deadline"
        );
        assert!(
            started.elapsed() < TRANSFER_TIMEOUT * 4,
            "it hung well past the deadline: {:?}",
            started.elapsed()
        );
    }
}
