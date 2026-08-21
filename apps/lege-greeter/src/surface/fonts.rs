//! The one embedded font this surface draws with — the same DejaVu Sans
//! `shell/lege-shell/src/surface/fonts.rs` vendors, copied rather than shared: crates do not share
//! `assets/`, and a greeter that depended on `lege-shell` for a font file would be a strange new
//! coupling between two binaries that otherwise share nothing but a toolkit. See that file's own
//! doc comment for why this particular font (permissively licensed, a single TrueType `glyf` face,
//! covers everything this surface draws) and `assets/fonts/LICENSE` beside it here for the terms.

use std::sync::OnceLock;

use pixelkit_text::{Face, FaceId, FontSet};

/// The (only) face this surface has.
pub const BODY: FaceId = FaceId(0);

fn face() -> &'static Face {
    static FACE: OnceLock<Face> = OnceLock::new();
    FACE.get_or_init(|| {
        Face::from_bytes(include_bytes!("../../assets/fonts/DejaVuSans.ttf"))
            .expect("the embedded DejaVu Sans font is a well-formed TrueType glyf face")
    })
}

pub fn set() -> &'static FontSet {
    static SET: OnceLock<FontSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut set = FontSet::new();
        set.add(face());
        set
    })
}
