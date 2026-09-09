use std::sync::OnceLock;

use pixelkit_text::{Face, FaceId, FontSet};

pub const BODY: FaceId = FaceId(0);

fn face() -> &'static Face {
    static FACE: OnceLock<Face> = OnceLock::new();
    FACE.get_or_init(|| {
        Face::from_bytes(include_bytes!(
            "../../../shell/templar-shell/assets/fonts/DejaVuSans.ttf"
        ))
        .expect("the embedded DejaVu Sans font is valid")
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
