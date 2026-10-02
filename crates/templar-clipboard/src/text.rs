//! What a text copy puts down, and what a text paste picks up.
//!
//! A text selection is offered under several MIME types at once because the receivers disagree
//! about the spelling: native Wayland clients (GTK, Qt, Chromium, `wl-copy`) ask for
//! `text/plain;charset=utf-8` or bare `text/plain`, while anything arriving over the XWayland
//! bridge offers the ICCCM atom names `UTF8_STRING`, `TEXT` and `STRING`. The bytes behind all
//! of them are the same UTF-8 text.
//!
//! Prepared up front rather than rendered on demand, following the file clipboard's lead: a
//! `send` request arrives on the Wayland queue, possibly long after the copy, and the selection
//! must be whatever was copied *then* even if the person has since typed something else.

/// `text/plain;charset=utf-8`: what `wl-copy`, GTK, Qt and Chromium offer and ask for first.
pub const TEXT_PLAIN_UTF8: &str = "text/plain;charset=utf-8";
/// Bare `text/plain`: what older senders emit; read as UTF-8 like everything else here.
pub const TEXT_PLAIN: &str = "text/plain";
/// The ICCCM atom name for UTF-8 text, carried over the XWayland bridge.
pub const UTF8_STRING: &str = "UTF8_STRING";
/// Legacy X11 text atom; arrives from the oldest X clients.
pub const TEXT: &str = "TEXT";
/// Legacy X11 text atom; the last resort, after [`TEXT`].
pub const STRING: &str = "STRING";

/// A text waiting to be pasted.
///
/// `None` for an empty string: an empty clipboard is not a clipboard, and putting one down
/// would silently discard whatever was on it. Mirrors the file clipboard's rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextClipboard {
    text: String,
}

impl TextClipboard {
    /// Copy `text`; `None` when it holds nothing to paste.
    pub fn set(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        (!text.is_empty()).then_some(Self { text })
    }

    /// The copied text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The payload to serve for every offered MIME type.
    pub fn payload(&self) -> TextPayload {
        TextPayload {
            text: self.text.clone(),
        }
    }
}

/// The bytes to serve for each offered MIME type, prepared before the selection is set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextPayload {
    text: String,
}

impl TextPayload {
    /// Serve `clipboard`'s text under every type.
    pub fn of(clipboard: &TextClipboard) -> Self {
        clipboard.payload()
    }

    /// The MIME types to offer, in the order they are advertised.
    ///
    /// The charset-qualified `text/plain` first because a receiver that understands both should
    /// prefer the one that says what the bytes are; the X11 atom names last, in
    /// `UTF8_STRING`/`TEXT`/`STRING` fidelity order, so a bridge client still gets the text.
    pub fn offered_mime_types() -> [&'static str; 5] {
        [TEXT_PLAIN_UTF8, TEXT_PLAIN, UTF8_STRING, TEXT, STRING]
    }

    /// What to write when a receiver asks for `mime_type`, or `None` for one never offered.
    ///
    /// Matching is ASCII case-insensitive: MIME types are case-insensitive and at least one
    /// sender in the wild capitalises them. The served bytes are always the UTF-8 text.
    pub fn bytes_for(&self, mime_type: &str) -> Option<&[u8]> {
        TextTarget::of(mime_type).map(|_| self.text.as_bytes())
    }
}

/// The target to ask a foreign offer for, chosen from what it advertises.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextTarget {
    /// Charset-qualified plain text; the first choice whenever it is offered.
    PlainUtf8,
    /// Bare plain text.
    Plain,
    /// X11 UTF-8 atom, from across the bridge.
    Utf8String,
    /// Legacy X11 atom.
    Text,
    /// Legacy X11 atom; the last resort.
    String,
}

impl TextTarget {
    /// The MIME type this target asks for.
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::PlainUtf8 => TEXT_PLAIN_UTF8,
            Self::Plain => TEXT_PLAIN,
            Self::Utf8String => UTF8_STRING,
            Self::Text => TEXT,
            Self::String => STRING,
        }
    }

    /// Classify one offered MIME type, or `None` when it carries no text.
    fn of(mime_type: &str) -> Option<Self> {
        if mime_type.eq_ignore_ascii_case(TEXT_PLAIN_UTF8) {
            Some(Self::PlainUtf8)
        } else if mime_type.eq_ignore_ascii_case(TEXT_PLAIN) {
            Some(Self::Plain)
        } else if mime_type.eq_ignore_ascii_case(UTF8_STRING) {
            Some(Self::Utf8String)
        } else if mime_type.eq_ignore_ascii_case(TEXT) {
            Some(Self::Text)
        } else if mime_type.eq_ignore_ascii_case(STRING) {
            Some(Self::String)
        } else {
            None
        }
    }

    /// Preference order: earlier wins. Follows [`TextPayload::offered_mime_types`], so an offer
    /// holding both spellings is read as the more explicit one rather than the legacy atom.
    fn preference(self) -> u8 {
        match self {
            Self::PlainUtf8 => 0,
            Self::Plain => 1,
            Self::Utf8String => 2,
            Self::Text => 3,
            Self::String => 4,
        }
    }
}

/// Pick the best target an offer advertises, or `None` when it holds no text.
///
/// A `text/uri-list` offer is deliberately *not* text: file drops are the file clipboard's job,
/// and reading a dragged file's URI as pastable text would paste a path where a drop belonged.
pub fn choose_target(mime_types: &[String]) -> Option<TextTarget> {
    mime_types
        .iter()
        .filter_map(|offered| TextTarget::of(offered))
        .min_by_key(|target| target.preference())
}

/// Turn a transferred payload into pastable text.
///
/// `None` for an empty transfer: copying nothing is not a copy, and an empty read — a sender
/// that never wrote, a transfer that arrived blank — must not wipe the paste.
pub fn from_transfer(_target: TextTarget, payload: &str) -> Option<String> {
    (!payload.is_empty()).then(|| payload.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_offers_charset_qualified_first_and_serves_the_same_bytes_under_every_type() {
        let clipboard = TextClipboard::set("ข้าวผัด 2").unwrap();
        assert_eq!(
            TextPayload::offered_mime_types(),
            [TEXT_PLAIN_UTF8, TEXT_PLAIN, UTF8_STRING, TEXT, STRING,],
            "the type that says what the bytes are is advertised first"
        );
        let payload = TextPayload::of(&clipboard);
        for mime_type in TextPayload::offered_mime_types() {
            assert_eq!(
                payload.bytes_for(mime_type),
                Some("ข้าวผัด 2".as_bytes()),
                "a receiver asking for {mime_type} gets the copied text, not a conversion"
            );
        }
        // A receiver asking for something never offered gets nothing, not an empty reply that
        // looks like an answer.
        assert_eq!(payload.bytes_for("text/uri-list"), None);
        assert_eq!(payload.bytes_for(""), None);
    }

    #[test]
    fn text_matching_ignores_case_but_serves_exact_bytes() {
        let payload = TextPayload::of(&TextClipboard::set("Hi").unwrap());
        assert_eq!(payload.bytes_for("TEXT/PLAIN"), Some("Hi".as_bytes()));
        assert_eq!(payload.bytes_for("utf8_string"), Some("Hi".as_bytes()));
    }

    #[test]
    fn text_asks_a_foreign_offer_for_the_most_explicit_type_it_carries() {
        // A native client offering both plain spellings: the qualified one wins.
        let both = vec![TEXT_PLAIN.to_owned(), TEXT_PLAIN_UTF8.to_owned()];
        assert_eq!(choose_target(&both), Some(TextTarget::PlainUtf8));
        // An X11 client across the bridge: UTF-8 before the legacy atoms.
        let x11 = vec![STRING.to_owned(), TEXT.to_owned(), UTF8_STRING.to_owned()];
        assert_eq!(choose_target(&x11), Some(TextTarget::Utf8String));
        // One legacy atom alone is still text.
        assert_eq!(choose_target(&[TEXT.to_owned()]), Some(TextTarget::Text));
        // A file drop is not text, even though a URI is spelled in characters.
        assert_eq!(
            choose_target(&["text/uri-list".to_owned()]),
            None,
            "reading a drop as text would paste a path where a drop belonged"
        );
        assert_eq!(choose_target(&["image/png".to_owned()]), None);
        assert_eq!(choose_target(&[]), None);
    }

    #[test]
    fn text_round_trips_multiline_content_through_any_target() {
        let copied = "first line\nsecond line\n";
        let clipboard = TextClipboard::set(copied).unwrap();
        assert_eq!(clipboard.text(), copied);
        let payload = clipboard.payload();
        // Whatever the receiver asked for, the paste is the exact text: no trimming, no
        // re-encoding, because a clipboard that edits is a clipboard that corrupts.
        for target in [
            TextTarget::PlainUtf8,
            TextTarget::Plain,
            TextTarget::Utf8String,
            TextTarget::Text,
            TextTarget::String,
        ] {
            let served =
                std::str::from_utf8(payload.bytes_for(target.mime_type()).unwrap()).unwrap();
            assert_eq!(from_transfer(target, served), Some(copied.to_owned()));
        }
    }

    #[test]
    fn text_of_nothing_is_no_clipboard_at_all() {
        assert_eq!(TextClipboard::set(""), None);
        // And an empty or unreadable transfer does not become a paste either.
        assert_eq!(from_transfer(TextTarget::PlainUtf8, ""), None);
        assert_eq!(from_transfer(TextTarget::Utf8String, ""), None);
    }

    #[test]
    fn text_targets_name_the_wire_types_they_ask_for() {
        assert_eq!(TextTarget::PlainUtf8.mime_type(), TEXT_PLAIN_UTF8);
        assert_eq!(TextTarget::Utf8String.mime_type(), UTF8_STRING);
        assert_eq!(TextTarget::String.mime_type(), STRING);
    }
}
