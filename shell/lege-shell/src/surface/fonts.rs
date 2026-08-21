//! The one embedded font this surface draws with.
//!
//! `pixelkit`'s own README is explicit that "applications embed their own fonts" — the toolkit
//! ships none. DejaVu Sans (`assets/fonts/DejaVuSans.ttf`, licence alongside it) is vendored for
//! that: permissively licensed (Bitstream Vera terms — free to embed and redistribute), a single
//! TrueType `glyf` face (`pixelkit_text::Face::from_bytes` reads `glyf` outlines only, not
//! CFF/OTF), and it already covers everything this surface draws — English UI chrome and POSIX
//! file paths. No fallback chain is registered: a wider script mix is exactly the kind of gap
//! `FontSet::set_fallbacks` exists to close later, not something this milestone's shell needs.

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
