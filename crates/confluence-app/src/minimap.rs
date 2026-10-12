//! The matrix minimap in the grid's top-left corner (spec: round 4 §3): the
//! whole matrix stretched to fill the corner, with the part on screen
//! outlined; clicking or dragging in it moves the view. Pure geometry here;
//! drawing is in grid_view. All sizes are in grid (cell-area) coordinates
//! unless named.

use std::collections::HashMap;

use eframe::egui::{Pos2, Rect, Vec2};

/// The map's margin inside the corner.
pub const INSET: f32 = 6.0;

/// Points of map per point of grid along each axis, for `content` in `mini`.
pub fn scale(mini: Rect, content: Vec2) -> Vec2 {
    if content.x <= 0.0 || content.y <= 0.0 {
        Vec2::ZERO
    } else {
        mini.size() / content
    }
}

/// The map: the whole corner (less its margin), whatever the matrix's shape;
/// each axis is scaled on its own so a lopsided matrix never becomes a sliver.
pub fn mini_rect(corner: Rect, content: Vec2) -> Rect {
    let room = corner.shrink(INSET);
    if content.x <= 0.0 || content.y <= 0.0 || !room.is_positive() {
        return Rect::from_center_size(corner.center(), Vec2::ZERO);
    }
    room
}

/// The outline of the part on screen: scrolled to `offset`, `view` grid
/// points visible; kept inside the map.
pub fn view_rect(mini: Rect, content: Vec2, offset: Vec2, view: Vec2) -> Rect {
    let k = scale(mini, content);
    let r = Rect::from_min_size(mini.min + offset * k, view * k);
    Rect::from_min_max(
        Pos2::new(r.min.x.clamp(mini.min.x, mini.max.x), r.min.y.clamp(mini.min.y, mini.max.y)),
        Pos2::new(r.max.x.clamp(mini.min.x, mini.max.x), r.max.y.clamp(mini.min.y, mini.max.y)),
    )
}

/// The scroll offset that centres the view on map point `p`, within the
/// scroll range (zero when everything is visible).
pub fn offset_for(mini: Rect, content: Vec2, view: Vec2, p: Pos2) -> Vec2 {
    let k = scale(mini, content);
    if k.x <= 0.0 || k.y <= 0.0 {
        return Vec2::ZERO;
    }
    let at = (p - mini.min) / k;
    let max = (content - view).max(Vec2::ZERO);
    (at - view / 2.0).clamp(Vec2::ZERO, max)
}

/// Whether the map moves the view this frame: on the press, a click, or a
/// drag that actually moved. A button merely held down leaves the view
/// alone, so the wheel can scroll meanwhile.
pub fn moves_view(pressed_here: bool, clicked: bool, drag_delta: Vec2) -> bool {
    pressed_here || clicked || drag_delta != Vec2::ZERO
}

/// The offset after the wheel moved `delta` while the map is held (egui
/// stops the grid's own wheel scrolling during any drag, so the map passes
/// the wheel on), within the scroll range.
pub fn wheel_offset(offset: Vec2, delta: Vec2, content: Vec2, view: Vec2) -> Vec2 {
    (offset - delta).clamp(Vec2::ZERO, (content - view).max(Vec2::ZERO))
}

/// Where the view goes this frame: to `jump` (where the map was pressed,
/// clicked or dragged to) if any, then on by the `wheel` while held.
pub fn target(jump: Option<Vec2>, offset: Vec2, wheel: Vec2, content: Vec2, view: Vec2) -> Option<Vec2> {
    if wheel == Vec2::ZERO {
        jump
    } else {
        Some(wheel_offset(jump.unwrap_or(offset), wheel, content, view))
    }
}

/// Route dots for the map: each point (a cell centre in grid coordinates,
/// with its row band) placed at its map offset `p * k`. Points landing in
/// the same `bucket`-sized square of the map share one dot at the square's
/// centre, coloured by the lowest band among them, so the map draws at most
/// one dot per square however many routes exist, the same whatever their order.
pub fn dots(points: impl IntoIterator<Item = (Vec2, usize)>, k: Vec2, bucket: f32) -> Vec<(Vec2, usize)> {
    let bucket = bucket.max(0.5);
    let mut squares: HashMap<(i64, i64), usize> = HashMap::new();
    let mut out: Vec<(Vec2, usize)> = Vec::new();
    for (p, band) in points {
        let at = p * k;
        let sq = ((at.x / bucket).floor() as i64, (at.y / bucket).floor() as i64);
        match squares.get(&sq) {
            Some(&i) => out[i].1 = out[i].1.min(band),
            None => {
                squares.insert(sq, out.len());
                let centre = Vec2::new(sq.0 as f32 + 0.5, sq.1 as f32 + 0.5) * bucket;
                out.push((centre, band));
            }
        }
    }
    out
}

/// The map's dots, kept between frames and rebuilt only when their `key`
/// (routes, layout and map size) changes.
#[derive(Default)]
pub struct DotCache {
    key: Option<u64>,
    dots: Vec<(Vec2, usize)>,
    /// How many times the dots were built (for tests).
    pub builds: usize,
}

impl DotCache {
    pub fn get(&mut self, key: u64, build: impl FnOnce() -> Vec<(Vec2, usize)>) -> &[(Vec2, usize)] {
        if self.key != Some(key) {
            self.dots = build();
            self.key = Some(key);
            self.builds += 1;
        }
        &self.dots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corner() -> Rect {
        Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(170.0, 128.0))
    }

    #[test]
    fn the_map_fills_the_corner_whatever_the_matrix_shape() {
        for content in [Vec2::new(1000.0, 500.0), Vec2::new(100.0, 4000.0), Vec2::new(9000.0, 40.0)] {
            assert_eq!(mini_rect(corner(), content), corner().shrink(INSET), "{content:?}");
        }
    }

    #[test]
    fn an_empty_matrix_has_an_empty_map_and_never_divides_by_zero() {
        let m = mini_rect(corner(), Vec2::ZERO);
        assert_eq!(m.size(), Vec2::ZERO);
        let v = view_rect(m, Vec2::ZERO, Vec2::ZERO, Vec2::new(300.0, 300.0));
        assert!(v.min.x.is_finite() && v.max.y.is_finite());
        assert_eq!(offset_for(m, Vec2::ZERO, Vec2::new(300.0, 300.0), m.center()), Vec2::ZERO);
    }

    #[test]
    fn the_view_is_outlined_on_each_axis_and_clamped_inside_the_map() {
        // Lopsided: 20 times wider than tall.
        let content = Vec2::new(8000.0, 400.0);
        let m = mini_rect(corner(), content);
        let v = view_rect(m, content, Vec2::new(4000.0, 100.0), Vec2::new(2000.0, 100.0));
        assert!((v.left() - m.center().x).abs() < 0.01, "half way across");
        assert!((v.width() - m.width() / 4.0).abs() < 0.01);
        assert!((v.top() - (m.top() + m.height() / 4.0)).abs() < 0.01, "a quarter down");
        assert!((v.height() - m.height() / 4.0).abs() < 0.01);
        let all = view_rect(m, content, Vec2::ZERO, Vec2::new(50000.0, 50000.0));
        assert_eq!(all, m, "a view larger than the matrix covers the whole map, no more");
    }

    #[test]
    fn a_click_centres_the_view_there_on_each_axis_within_the_scroll_range() {
        let content = Vec2::new(8000.0, 400.0);
        let view = Vec2::new(200.0, 100.0);
        let m = mini_rect(corner(), content);
        let o = offset_for(m, content, view, m.center());
        assert!((o - Vec2::new(3900.0, 150.0)).length() < 0.01, "{o:?}");
        let quarter = offset_for(m, content, view, m.min + m.size() / 4.0);
        assert!((quarter - Vec2::new(1900.0, 50.0)).length() < 0.01, "{quarter:?}");
        assert_eq!(offset_for(m, content, view, m.min), Vec2::ZERO, "clamped at the start");
        assert_eq!(offset_for(m, content, view, m.max + Vec2::splat(50.0)), content - view, "and at the end");
        assert_eq!(offset_for(m, content, Vec2::splat(40000.0), m.center()), Vec2::ZERO, "all visible: no scrolling");
    }

    #[test]
    fn only_a_press_a_click_or_a_real_drag_moves_the_view() {
        assert!(moves_view(true, false, Vec2::ZERO), "the press");
        assert!(moves_view(false, true, Vec2::ZERO), "a click");
        assert!(moves_view(false, false, Vec2::new(0.0, 2.0)), "dragging");
        assert!(!moves_view(false, false, Vec2::ZERO), "merely held: the wheel may scroll");
    }

    #[test]
    fn the_wheel_scrolls_on_while_the_map_is_held() {
        let (content, view) = (Vec2::new(1000.0, 800.0), Vec2::new(200.0, 100.0));
        let down = wheel_offset(Vec2::new(50.0, 100.0), Vec2::new(0.0, -60.0), content, view);
        assert_eq!(down, Vec2::new(50.0, 160.0));
        assert_eq!(
            wheel_offset(Vec2::new(0.0, 690.0), Vec2::new(0.0, -60.0), content, view).y,
            700.0,
            "stops at the end"
        );
        assert_eq!(wheel_offset(Vec2::new(0.0, 10.0), Vec2::new(0.0, 60.0), content, view).y, 0.0, "and the start");
    }

    #[test]
    fn a_press_with_the_wheel_in_the_same_frame_jumps_then_scrolls() {
        let (content, view) = (Vec2::new(1000.0, 800.0), Vec2::new(200.0, 100.0));
        let (offset, wheel) = (Vec2::new(10.0, 10.0), Vec2::new(0.0, -60.0));
        let jump = Some(Vec2::new(400.0, 300.0));
        assert_eq!(target(jump, offset, wheel, content, view), Some(Vec2::new(400.0, 360.0)));
        assert_eq!(target(None, offset, wheel, content, view), Some(Vec2::new(10.0, 70.0)), "held: the wheel");
        assert_eq!(target(jump, offset, Vec2::ZERO, content, view), jump, "the press alone");
        assert_eq!(target(None, offset, Vec2::ZERO, content, view), None, "nothing to do");
    }

    #[test]
    fn routes_on_the_same_spot_of_the_map_share_one_dot() {
        let k = Vec2::new(0.1, 0.05);
        let points = vec![
            (Vec2::new(100.0, 100.0), 0),
            (Vec2::new(110.0, 110.0), 1), // 1 pt right, half a point down on the map: same square
            (Vec2::new(500.0, 100.0), 2),
        ];
        let d = dots(points, k, 2.0);
        assert_eq!(d.len(), 2, "{d:?}");
        assert_eq!(d[0], (Vec2::new(11.0, 5.0), 0));
        assert_eq!(d[1].1, 2);
        // A huge matrix full of routes still draws at most one dot per square.
        let many = (0..200).flat_map(|r| (0..200).map(move |c| (Vec2::new(c as f32 * 22.0, r as f32 * 22.0), 0)));
        let n = dots(many, Vec2::new(158.0 / 4400.0, 116.0 / 4400.0), 2.0).len();
        assert!(n <= 80 * 59, "{n} dots");
    }

    #[test]
    fn a_shared_dot_takes_the_lowest_devices_colour_whatever_the_order() {
        let k = Vec2::splat(0.1);
        let a = vec![(Vec2::new(100.0, 100.0), 2), (Vec2::new(101.0, 101.0), 0), (Vec2::new(102.0, 101.0), 1)];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(dots(a, k, 2.0), vec![(Vec2::new(11.0, 11.0), 0)], "at the square's centre");
        assert_eq!(dots(b, k, 2.0)[0].1, 0, "the routes' order does not matter");
    }

    #[test]
    fn the_dots_are_rebuilt_only_when_their_key_changes() {
        let mut c = DotCache::default();
        let one = || vec![(Vec2::ZERO, 0)];
        assert_eq!(c.get(7, one).len(), 1);
        assert_eq!(c.get(7, || panic!("rebuilt with the same key")).len(), 1);
        assert_eq!(c.get(8, Vec::new).len(), 0);
        assert_eq!(c.builds, 2);
    }
}
