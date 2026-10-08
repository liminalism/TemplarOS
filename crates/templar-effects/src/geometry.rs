//! Logical-pixel rectangles. Deliberately not `templar_window::Rect`: the effect library takes
//! plain numbers so a host on any side of the compositor/shell split can feed it.

/// An axis-aligned rectangle in logical pixels: origin at the top-left, `w`/`h` never negative
/// in a well-formed value (an empty rectangle has a zero side).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub const fn right(self) -> i32 {
        self.x.saturating_add(self.w)
    }

    #[must_use]
    pub const fn bottom(self) -> i32 {
        self.y.saturating_add(self.h)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Half-open containment of a sub-pixel point: the left/top edges are inside, the
    /// right/bottom edges are not, so two abutting rectangles never both claim a point.
    #[must_use]
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x as f32
            && y >= self.y as f32
            && x < self.right() as f32
            && y < self.bottom() as f32
    }

    /// The overlap of two rectangles, or an empty rectangle at `self`'s origin.
    #[must_use]
    pub fn intersect(self, other: Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r <= x || b <= y {
            Rect::new(self.x, self.y, 0, 0)
        } else {
            Rect::new(x, y, r - x, b - y)
        }
    }

    /// The smallest rectangle holding both; an empty operand contributes nothing.
    #[must_use]
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    #[must_use]
    pub fn center(self) -> (f32, f32) {
        (
            self.x as f32 + self.w as f32 * 0.5,
            self.y as f32 + self.h as f32 * 0.5,
        )
    }

    /// `[x, y, w, h]` as floats, the layout every shader uniform in this crate uses.
    #[must_use]
    pub fn to_f32(self) -> [f32; 4] {
        [self.x as f32, self.y as f32, self.w as f32, self.h as f32]
    }
}

#[cfg(test)]
mod tests {
    use super::Rect;

    #[test]
    fn intersect_and_union() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(b), Rect::new(5, 5, 5, 5));
        assert!(a.intersect(Rect::new(10, 0, 5, 5)).is_empty());
        assert_eq!(a.union(b), Rect::new(0, 0, 15, 15));
        assert_eq!(Rect::default().union(b), b);
    }

    #[test]
    fn containment_is_half_open() {
        let a = Rect::new(0, 0, 10, 10);
        assert!(a.contains(0.0, 0.0));
        assert!(!a.contains(10.0, 5.0));
        assert!(a.contains(9.99, 9.99));
    }
}
