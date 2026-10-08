//! The WGSL sources, assembled. WGSL has no includes, so each module is `common.wgsl`
//! followed by the module's own file, concatenated at compile time.
//!
//! The host binds them as each file's header documents. Nothing here compiles WGSL: the
//! sandbox validates every module with naga (and translates it to GLSL ES 3.0 and SPIR-V)
//! in `effects-sandbox --check`, which is the gate for an edit here.

/// One shader module and its entry points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Module {
    pub name: &'static str,
    pub source: &'static str,
    pub vertex: &'static [&'static str],
    pub fragment: &'static [&'static str],
}

/// The shared helpers alone, for a host's own small shaders (prepend it, as the modules do).
pub const COMMON: &str = include_str!("../shaders/common.wgsl");

/// The essence transition: `vs_body`/`fs_body` and `vs_mote`/`fs_mote`.
pub const ESSENCE: Module = Module {
    name: "essence",
    source: concat!(
        include_str!("../shaders/common.wgsl"),
        include_str!("../shaders/essence.wgsl")
    ),
    vertex: &["vs_body", "vs_mote"],
    fragment: &["fs_body", "fs_mote"],
};

/// Surface materials: `vs_surface`/`fs_surface`.
pub const SURFACE: Module = Module {
    name: "surface",
    source: concat!(
        include_str!("../shaders/common.wgsl"),
        include_str!("../shaders/surface.wgsl")
    ),
    vertex: &["vs_surface"],
    fragment: &["fs_surface"],
};

/// Bottom-bar plates: `vs_bar`/`fs_bar`.
pub const BAR: Module = Module {
    name: "bar",
    source: concat!(
        include_str!("../shaders/common.wgsl"),
        include_str!("../shaders/bar.wgsl")
    ),
    vertex: &["vs_bar"],
    fragment: &["fs_bar"],
};

pub const ALL: &[Module] = &[ESSENCE, SURFACE, BAR];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_point_is_in_its_source() {
        for module in ALL {
            for entry in module.vertex.iter().chain(module.fragment) {
                assert!(
                    module.source.contains(&format!("fn {entry}(")),
                    "{} lacks {entry}",
                    module.name
                );
            }
        }
    }

    #[test]
    fn uniform_sizes_match_the_structs() {
        // Each struct is vec4-aligned throughout; count its members.
        let essence_vec4s = 8 + crate::essence::MAX_OBSTACLES;
        assert_eq!(crate::essence::UNIFORM_BYTES, essence_vec4s * 16);
        assert!(ESSENCE.source.contains(&format!(
            "obstacles: array<vec4<f32>, {}>",
            crate::essence::MAX_OBSTACLES
        )));
        assert_eq!(crate::surface::UNIFORM_BYTES, 7 * 16);
        assert_eq!(crate::bar::UNIFORM_BYTES, 6 * 16);
    }
}
