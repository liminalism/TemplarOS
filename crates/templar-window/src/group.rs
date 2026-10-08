//! Window groups: an explicit, user-made set of two or more windows that behave as one unit of
//! work (`docs/planning/window-groups-and-application-integration.md` §2).
//!
//! Nothing here infers a group. A group exists because the user made one — by dropping a window's
//! band on another's, or by a chord — and it dissolves when it would be left with fewer than two
//! members. That is the whole of the model's policy; the rest is identity ([`GroupId`]), a name,
//! and the accent colour every surface uses to draw the same group the same way.

use crate::WindowId;
use serde::{Deserialize, Serialize};

/// A window group's identity. Ids start at 1 and are never reused: 0 means "no group" on the wire
/// (`interfaces/wayland/templar-shell-v2.xml`'s `group` event), so it must never name a live group.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct GroupId(pub u64);

/// The six group accents, as 0x00RRGGBB.
///
/// Chosen to be distinguishable from one another *and* legible as a 2-logical-pixel outline and a
/// 4-logical-pixel band stripe on the shell's off-white ground: six hues roughly 60° apart, all
/// held at a mid-dark lightness so each has real contrast against both the light field and the
/// dark recovery band, and none of them reads as an error or a warning (which the shell reserves
/// for semantic state). In order: blue, teal, green, amber, crimson, violet.
pub const GROUP_ACCENTS: [u32; 6] = [
    0x002F_6FD0, // blue
    0x0015_7F73, // teal
    0x004B_7F1F, // green
    0x00B5_721A, // amber
    0x00B3_3A4B, // crimson
    0x007A_4FBF, // violet
];

/// The accent a group of this id wears. Indexed by the id itself so the colour is a fact about
/// the group rather than about the order the shell happened to learn of it: the workspace, the
/// switcher and Home can never disagree, even if one of them restarted.
#[must_use]
pub fn accent_for(group: GroupId) -> u32 {
    let index = group.0.wrapping_sub(1) % GROUP_ACCENTS.len() as u64;
    GROUP_ACCENTS[index as usize]
}

/// What the service knows about one live group. Membership itself lives on the windows
/// ([`crate::WindowRecord::group`]), so a window can never be in two groups by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRecord {
    /// What the user called it, if anything. `None` renders as the empty string on the wire.
    pub name: Option<String>,
    /// 0x00RRGGBB, from [`accent_for`].
    pub accent: u32,
    /// The member `Super+Tab` lands on when it reaches this group. Kept pointing at a live
    /// member: a member that leaves or closes hands it to another one.
    pub last_active: WindowId,
}

impl GroupRecord {
    /// The name as the shell receives it: the empty string for an unnamed group.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }
}

/// Logical width of the accent stripe a grouped window wears at the leading edge of its
/// horizontal title tab (planning doc §2 rule 7: "a stripe of the same colour in each
/// member's band"). Titles are always horizontal, so the stripe never rotates with the
/// corner — paint scales this width into pixels at the tab's leading edge.
pub const GROUP_STRIPE_WIDTH: i32 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_cycle_through_the_palette_from_the_first_group() {
        assert_eq!(accent_for(GroupId(1)), GROUP_ACCENTS[0]);
        assert_eq!(accent_for(GroupId(6)), GROUP_ACCENTS[5]);
        assert_eq!(accent_for(GroupId(7)), GROUP_ACCENTS[0]);
    }

    /// Six *distinct* colours, none of them so dark or so light that a hairline outline of it
    /// disappears into either the light field or the dark band.
    #[test]
    fn the_palette_is_six_distinct_mid_toned_colours() {
        let mut seen = std::collections::BTreeSet::new();
        for accent in GROUP_ACCENTS {
            assert!(seen.insert(accent), "{accent:#08x} appears twice");
            assert_eq!(accent & 0xFF00_0000, 0, "accents are 0x00RRGGBB");
            let (red, green, blue) = (
                f32::from(u8::try_from((accent >> 16) & 0xFF).unwrap()),
                f32::from(u8::try_from((accent >> 8) & 0xFF).unwrap()),
                f32::from(u8::try_from(accent & 0xFF).unwrap()),
            );
            let luminance = 0.2126f32.mul_add(red, 0.7152f32.mul_add(green, 0.0722 * blue));
            assert!(
                (40.0..190.0).contains(&luminance),
                "{accent:#08x} is too dark or too light to read on both grounds"
            );
        }
    }

    #[test]
    fn an_unnamed_group_renders_as_the_empty_string() {
        let record = GroupRecord {
            name: None,
            accent: accent_for(GroupId(1)),
            last_active: WindowId(1),
        };
        assert_eq!(record.display_name(), "");
    }

    /// The stripe fits inside every corner's title tab — paint's leading-edge stripe is
    /// always drawable, never wider than the tab.
    #[test]
    fn the_group_stripe_fits_every_corner_s_title() {
        use crate::geometry::{Corner, TabPlacement, WindowLayout};
        let client = crate::geometry::Rect::new(300, 300, 640, 480);
        let output = crate::geometry::Rect::new(0, 0, 1920, 1080);
        for corner in Corner::ALL {
            let layout = WindowLayout::new(client, corner, TabPlacement::SameCorner, output);
            assert!(
                GROUP_STRIPE_WIDTH < layout.title.width,
                "stripe overflows the {corner:?} title"
            );
        }
    }
}
