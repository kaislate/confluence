//! The matrix minimap in the grid's top-left corner (spec: round 4 §3): the
//! whole matrix scaled to fit, with the part on screen outlined; clicking or
//! dragging in it moves the view. Pure geometry here; drawing is in
//! grid_view. All sizes are in grid (cell-area) coordinates unless named.

use eframe::egui::{Pos2, Rect, Vec2};

/// The map's margin inside the corner.
pub const INSET: f32 = 8.0;

/// Points of map per point of grid, for `content` in `mini`.
fn scale(mini: Rect, content: Vec2) -> f32 {
    if content.x <= 0.0 || content.y <= 0.0 {
        0.0
    } else {
        mini.width() / content.x
    }
}

/// The map: the matrix (`content` grid points) scaled to fit `corner`, its
/// aspect kept, centred.
pub fn mini_rect(corner: Rect, content: Vec2) -> Rect {
    let room = corner.shrink(INSET);
    if content.x <= 0.0 || content.y <= 0.0 || !room.is_positive() {
        return Rect::from_center_size(corner.center(), Vec2::ZERO);
    }
    let k = (room.width() / content.x).min(room.height() / content.y);
    Rect::from_center_size(room.center(), content * k)
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
    if k <= 0.0 {
        return Vec2::ZERO;
    }
    let at = (p - mini.min) / k;
    let max = (content - view).max(Vec2::ZERO);
    (at - view / 2.0).clamp(Vec2::ZERO, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corner() -> Rect {
        Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(160.0, 96.0))
    }

    #[test]
    fn the_map_keeps_the_matrix_aspect_and_sits_centred_in_the_corner() {
        let m = mini_rect(corner(), Vec2::new(1000.0, 500.0));
        assert!((m.width() / m.height() - 2.0).abs() < 1e-3);
        assert!(corner().shrink(INSET - 0.01).contains_rect(m));
        assert!((m.center() - corner().center()).length() < 0.01);
        let tall = mini_rect(corner(), Vec2::new(100.0, 2000.0));
        assert!((tall.height() - (96.0 - 2.0 * INSET)).abs() < 1e-3, "limited by height");
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
    fn the_view_is_outlined_and_clamped_inside_the_map() {
        let content = Vec2::new(1000.0, 500.0);
        let m = mini_rect(corner(), content);
        let v = view_rect(m, content, Vec2::new(500.0, 0.0), Vec2::new(250.0, 250.0));
        assert!((v.left() - m.center().x).abs() < 0.01, "half way across");
        assert!((v.width() - m.width() / 4.0).abs() < 0.01);
        // A view larger than the matrix covers the whole map, no more.
        let all = view_rect(m, content, Vec2::ZERO, Vec2::new(5000.0, 5000.0));
        assert_eq!(all, m);
    }

    #[test]
    fn a_click_centres_the_view_there_within_the_scroll_range() {
        let content = Vec2::new(1000.0, 500.0);
        let view = Vec2::new(200.0, 100.0);
        let m = mini_rect(corner(), content);
        let o = offset_for(m, content, view, m.center());
        assert!((o - Vec2::new(400.0, 200.0)).length() < 0.01, "{o:?}");
        assert_eq!(offset_for(m, content, view, m.min), Vec2::ZERO, "clamped at the start");
        assert_eq!(offset_for(m, content, view, m.max + Vec2::splat(50.0)), content - view, "and at the end");
        assert_eq!(offset_for(m, content, Vec2::splat(4000.0), m.center()), Vec2::ZERO, "all visible: no scrolling");
    }
}
