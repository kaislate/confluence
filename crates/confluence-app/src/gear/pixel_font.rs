//! A 3×5 pixel font for OLED meters: the numbers, scale and labels look
//! like they are lit on the same display as the bars.

use eframe::egui::epaint::Mesh;
use eframe::egui::{Color32, Pos2, Rect, Vec2};

/// Glyph width and height in pixels, and the advance (width + 1 spacing).
pub const W: usize = 3;
pub const H: usize = 5;
pub const ADVANCE: usize = W + 1;

/// The rows of `c` as 3-bit masks (bit 2 is the left pixel), top to bottom.
pub fn glyph(c: char) -> Option<[u8; H]> {
    let g: [u8; H] = match c.to_ascii_uppercase() {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        '+' => [0b000, 0b010, 0b111, 0b010, 0b000],
        '.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        ':' => [0b000, 0b010, 0b000, 0b010, 0b000],
        '/' => [0b001, 0b001, 0b010, 0b100, 0b100],
        ' ' => [0; H],
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'C' => [0b011, 0b100, 0b100, 0b100, 0b011],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
        'G' => [0b011, 0b100, 0b101, 0b101, 0b011],
        'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
        'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
        'J' => [0b001, 0b001, 0b001, 0b101, 0b010],
        'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'N' => [0b110, 0b101, 0b101, 0b101, 0b101],
        'O' => [0b010, 0b101, 0b101, 0b101, 0b010],
        'P' => [0b110, 0b101, 0b110, 0b100, 0b100],
        'Q' => [0b010, 0b101, 0b101, 0b110, 0b011],
        'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        'S' => [0b011, 0b100, 0b010, 0b001, 0b110],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        'V' => [0b101, 0b101, 0b101, 0b101, 0b010],
        'W' => [0b101, 0b101, 0b111, 0b111, 0b101],
        'X' => [0b101, 0b101, 0b010, 0b101, 0b101],
        'Y' => [0b101, 0b101, 0b010, 0b010, 0b010],
        'Z' => [0b111, 0b001, 0b010, 0b100, 0b111],
        _ => return None,
    };
    Some(g)
}

/// Width of `text` in screen points at `px` points per pixel.
pub fn width(text: &str, px: f32) -> f32 {
    let n = text.chars().count();
    if n == 0 {
        0.0
    } else {
        (n * ADVANCE - 1) as f32 * px
    }
}

/// Adds `text` to `mesh` with its top-left at `pos`, `px` points per pixel.
/// `dot` leaves a gap between pixels (the dot-matrix look). Unknown
/// characters are drawn as spaces.
pub fn draw(mesh: &mut Mesh, pos: Pos2, text: &str, px: f32, color: Color32, dot: bool) {
    let inset = if dot { px * 0.2 } else { 0.0 };
    let mut x = pos.x;
    for c in text.chars() {
        if let Some(g) = glyph(c) {
            for (row, bits) in g.iter().enumerate() {
                for col in 0..W {
                    if bits & (1 << (W - 1 - col)) != 0 {
                        let min = Pos2::new(x + col as f32 * px, pos.y + row as f32 * px);
                        let r = Rect::from_min_size(min, Vec2::splat(px - inset));
                        mesh.add_colored_rect(r, color);
                    }
                }
            }
        }
        x += ADVANCE as f32 * px;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_characters_are_all_there() {
        for c in "0123456789-+.:/ INOUTCLIPFDB".chars() {
            assert!(glyph(c).is_some(), "{c:?}");
        }
        assert!(glyph('a').is_some(), "lower case maps to upper");
        assert!(glyph('#').is_none());
        for c in ('A'..='Z').chain('0'..='9') {
            assert!(glyph(c).unwrap().iter().all(|r| *r < 8), "{c} fits 3 columns");
        }
    }

    #[test]
    fn widths_count_glyphs_and_gaps() {
        assert_eq!(width("", 1.0), 0.0);
        assert_eq!(width("8", 1.0), 3.0);
        assert_eq!(width("12", 2.0), 14.0);
    }

    #[test]
    fn drawing_lights_the_glyphs_pixels() {
        let mut m = Mesh::default();
        draw(&mut m, Pos2::ZERO, "1", 1.0, Color32::WHITE, false);
        // "1" has 8 lit pixels: 4 vertices each.
        assert_eq!(m.vertices.len(), 8 * 4);
    }
}
