//! Light and shade maps for the "narrow S" knob, computed from its shape: a
//! port of the design study's `s_maps.py`, with the same structure and
//! constants so the two can be compared.
//!
//! - shade: 255 = no change, darker where the slope faces away from the
//!   light or lies in the knob's cast shadow (drawn as black with alpha);
//! - light: 0 = no change, brighter where the slope faces the light, plus a
//!   faint spill onto the panel beside it (added as the panel's own colour);
//! - cap: the cap's coverage, supersampled so its edge is antialiased.
//!
//! The flat panel is exactly neutral in all three, so the maps have no edges.

use std::f32::consts::PI;
use std::sync::OnceLock;

/// The knob is drawn smaller than its map so its cast shadow fits.
const SCALE: f32 = 0.68;
/// Cap radius, as a fraction of the map width.
const R_CAP: f32 = 0.30 * SCALE;
/// Width of the S slope (narrow, a little wider to show the curve).
const WIDTH: f32 = 0.185 * SCALE;
/// Height of the S (drives steepness and shadow length).
const HEIGHT: f32 = 0.11 * SCALE;
/// The cap's gentle dome.
const DOME: f32 = 0.012 * SCALE;
/// Darkest cast shadow (multiply).
const K_CAST: f32 = 0.48;
/// The cast shadow is soft: computed at low resolution, then interpolated.
const CAST_RES: usize = 140;
/// Strength of lightening and darkening (balanced by eye).
const K_LIGHT: f32 = 0.95;
const K_SHADE: f32 = 0.62;
/// Light bounced off the lit slope onto the panel, and how far it reaches.
const K_SPILL: f32 = 0.06;
const SPILL_FALLOFF: f32 = 0.065;
/// Supersampling per axis (16 samples per pixel).
const SS: usize = 4;

/// The cap's diameter as a fraction of the map: where to draw the cap's
/// face and value text.
pub const CAP_FRACTION: f32 = 2.0 * R_CAP;

/// Light from the top-left, above (x right, y down, z up), normalised.
fn light() -> [f32; 3] {
    let l = [-0.62f32, -0.62, 0.48];
    let n = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
    [l[0] / n, l[1] / n, l[2] / n]
}

/// One byte per pixel, `size` x `size`, row by row.
#[derive(Clone, Debug)]
pub struct KnobMaps {
    pub size: usize,
    pub shade: Vec<u8>,
    pub light: Vec<u8>,
    pub cap: Vec<u8>,
}

fn height(r: f32) -> f32 {
    if r >= R_CAP + WIDTH {
        return 0.0;
    }
    if r >= R_CAP {
        // The S: a long concave foot, a shorter convex shoulder.
        let t = (R_CAP + WIDTH - r) / WIDTH;
        return HEIGHT * (1.0 - (PI * t.powf(1.35)).cos()) / 2.0;
    }
    HEIGHT + DOME * (1.0 - (r / R_CAP).powi(2)) // the cap: a gentle dome
}

fn dh_dr(r: f32) -> f32 {
    let e = 1e-4;
    (height(r + e) - height(r - e)) / (2.0 * e)
}

/// N·L minus the flat panel's N·L: 0 on the flat panel, + facing the light,
/// - facing away.
fn shade_at(x: f32, y: f32, l: [f32; 3]) -> f32 {
    let (dx, dy) = (x - 0.5, y - 0.5);
    let r = dx.hypot(dy);
    let (hx, hy) = if r < 1e-6 {
        (0.0, 0.0)
    } else {
        let g = dh_dr(r);
        (g * dx / r, g * dy / r)
    };
    let n = [-hx, -hy, 1.0];
    let nn = (n[0] * n[0] + n[1] * n[1] + 1.0).sqrt();
    (n[0] * l[0] + n[1] * l[1] + n[2] * l[2]) / nn - l[2]
}

/// Fraction of an area light (top-left) each point cannot see past the knob,
/// on a `CAST_RES` grid. It does not depend on the map size: computed once.
fn cast_shadow() -> &'static [f32] {
    static CAST: OnceLock<Vec<f32>> = OnceLock::new();
    CAST.get_or_init(|| {
        let l = light();
        let lxy = l[0].hypot(l[1]);
        let base_az = l[1].atan2(l[0]);
        let base_el = l[2].atan2(lxy);
        // A disc-shaped area light: 9 x 7 samples across azimuth and elevation.
        let samples: Vec<(f32, f32, f32)> = (0..9)
            .flat_map(|i| (0..7).map(move |j| (i, j)))
            .map(|(i, j)| {
                let az = base_az + 0.40 * (i as f32 / 8.0 - 0.5);
                let el = base_el + 0.26 * (j as f32 / 6.0 - 0.5);
                (az.cos(), az.sin(), el.tan())
            })
            .collect();
        let top = HEIGHT + DOME;
        let mut grid = vec![0.0f32; CAST_RES * CAST_RES];
        for gy in 0..CAST_RES {
            for gx in 0..CAST_RES {
                let (x, y) = ((gx as f32 + 0.5) / CAST_RES as f32, (gy as f32 + 0.5) / CAST_RES as f32);
                let r0 = (x - 0.5).hypot(y - 0.5);
                let h0 = height(r0);
                let mut blocked = 0;
                for &(dx, dy, rise) in &samples {
                    let mut t = 0.004f32;
                    while t < 0.30 {
                        if h0 + t * rise > top {
                            break; // the ray is above anything the knob has
                        }
                        let (hx, hy) = (x + dx * t, y + dy * t);
                        let rr = (hx - 0.5).hypot(hy - 0.5);
                        if rr > R_CAP + WIDTH && t > 0.02 && rr > r0 {
                            break; // past the knob, moving away: nothing more can block
                        }
                        if height(rr) > h0 + t * rise {
                            blocked += 1;
                            break;
                        }
                        t += 0.004;
                    }
                }
                grid[gy * CAST_RES + gx] = blocked as f32 / samples.len() as f32;
            }
        }
        // Smooth the remaining steps (three 3x3 box blurs).
        let at = |g: &[f32], x: isize, y: isize| {
            let c = |v: isize| v.clamp(0, CAST_RES as isize - 1) as usize;
            g[c(y) * CAST_RES + c(x)]
        };
        for _ in 0..3 {
            let mut next = vec![0.0f32; CAST_RES * CAST_RES];
            for gy in 0..CAST_RES as isize {
                for gx in 0..CAST_RES as isize {
                    let mut sum = 0.0;
                    for yy in gy - 1..=gy + 1 {
                        for xx in gx - 1..=gx + 1 {
                            sum += at(&grid, xx, yy);
                        }
                    }
                    next[gy as usize * CAST_RES + gx as usize] = sum / 9.0;
                }
            }
            grid = next;
        }
        grid
    })
}

/// The cast shadow at (x, y), interpolated bilinearly.
fn cast_at(cast: &[f32], x: f32, y: f32) -> f32 {
    let n = CAST_RES as f32;
    let (fx, fy) = (x * n - 0.5, y * n - 0.5);
    let x0 = (fx.floor() as isize).clamp(0, CAST_RES as isize - 2) as usize;
    let y0 = (fy.floor() as isize).clamp(0, CAST_RES as isize - 2) as usize;
    let (tx, ty) = ((fx - x0 as f32).clamp(0.0, 1.0), (fy - y0 as f32).clamp(0.0, 1.0));
    let g = |x: usize, y: usize| cast[y * CAST_RES + x];
    let a = g(x0, y0) * (1.0 - tx) + g(x0 + 1, y0) * tx;
    let b = g(x0, y0 + 1) * (1.0 - tx) + g(x0 + 1, y0 + 1) * tx;
    a * (1.0 - ty) + b * ty
}

fn sstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Light reflected off the lit slope onto the panel beside it: strongest at
/// the foot on the side facing the light, tapering with distance and round
/// the sides.
fn spill_at(x: f32, y: f32, l: [f32; 3]) -> f32 {
    let (dx, dy) = (x - 0.5, y - 0.5);
    let r = dx.hypot(dy);
    let base = R_CAP + WIDTH;
    if r < base * 0.82 {
        return 0.0;
    }
    let lxy = l[0].hypot(l[1]);
    let facing = (dx * l[0] + dy * l[1]) / (r * lxy); // 1 straight toward the light
    if facing <= 0.0 {
        return 0.0;
    }
    // Fades in across the foot of the slope, then out over the panel, and is
    // gone by the edge of the map (so the map has no seam with the panel).
    let rise = sstep(base * 0.82, base, r);
    let edge = sstep(0.0, 0.06, x.min(1.0 - x).min(y).min(1.0 - y));
    facing.powf(1.6) * rise * (-(r - base).max(0.0) / SPILL_FALLOFF).exp() * edge
}

/// The three maps at `size` x `size` pixels (use 2x the display size).
pub fn generate(size: usize) -> KnobMaps {
    let l = light();
    let cast = cast_shadow();
    let n = size * size;
    let (mut shade, mut lightm, mut cap) = (vec![255u8; n], vec![0u8; n], vec![0u8; n]);
    let base = R_CAP + WIDTH;
    let sz = size as f32;
    for py in 0..size {
        for px in 0..size {
            let (cx, cy) = ((px as f32 + 0.5) / sz, (py as f32 + 0.5) / sz);
            let r = (cx - 0.5).hypot(cy - 0.5);
            let i = py * size + px;
            let occ = cast_at(cast, cx, cy);
            let spill = spill_at(cx, cy, l);
            // Beyond the slope (plus a pixel's margin) the shape term is
            // zero: no supersampling there.
            let near = r < base + 1.5 / sz;
            let (mut s, mut inside) = (0.0f32, 0u32);
            if near {
                for sy in 0..SS {
                    for sx in 0..SS {
                        let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / sz;
                        let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / sz;
                        s += shade_at(x, y, l);
                        inside += u32::from((x - 0.5).hypot(y - 0.5) < R_CAP);
                    }
                }
                s /= (SS * SS) as f32;
            }
            let lit = s.max(0.0) * (1.0 - occ) + K_SPILL / K_LIGHT * spill; // no highlight in shadow
            lightm[i] = (255.0 * K_LIGHT * lit).round().min(255.0) as u8;
            let sh = (1.0 - K_SHADE * (-s).max(0.0)) * (1.0 - K_CAST * occ);
            shade[i] = (255.0 * sh).round().clamp(0.0, 255.0) as u8;
            cap[i] = (255.0 * inside as f32 / (SS * SS) as f32).round() as u8;
        }
    }
    KnobMaps { size, shade, light: lightm, cap }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_are_neutral_away_from_the_knob() {
        let m = generate(120);
        let px = |v: &Vec<u8>, x: usize, y: usize| v[y * m.size + x];
        for (x, y) in [(0, 0), (119, 0), (0, 119), (60, 0), (0, 60)] {
            assert_eq!(px(&m.shade, x, y), 255, "shade at ({x},{y})");
            assert_eq!(px(&m.light, x, y), 0, "light at ({x},{y})");
            assert_eq!(px(&m.cap, x, y), 0);
        }
        assert_eq!(px(&m.cap, 60, 60), 255, "the cap covers the centre");
    }

    #[test]
    fn light_falls_on_the_top_left_and_shade_on_the_bottom_right() {
        let m = generate(200);
        let at = |v: &Vec<u8>, fx: f32, fy: f32| v[(fy * 200.0) as usize * 200 + (fx * 200.0) as usize];
        let r = 0.30 * 0.68 + 0.185 * 0.68 * 0.5; // halfway up the slope
        let d = r / std::f32::consts::SQRT_2;
        assert!(at(&m.light, 0.5 - d, 0.5 - d) > 20, "lit top-left");
        assert!(at(&m.shade, 0.5 + d, 0.5 + d) < 220, "shaded bottom-right");
        assert!(at(&m.light, 0.5 + d, 0.5 + d) < 5);
        assert!(at(&m.shade, 0.5 - d, 0.5 - d) > 250);
    }

    #[test]
    fn the_cap_edge_is_antialiased() {
        let m = generate(200);
        let partial = m.cap.iter().filter(|&&c| c > 0 && c < 255).count();
        assert!(partial > 100, "{partial} edge pixels have partial coverage");
    }

    #[test]
    fn generating_a_display_size_is_fast_enough() {
        let t = std::time::Instant::now();
        let _ = generate(170 * 2);
        assert!(t.elapsed() < std::time::Duration::from_millis(1500), "{:?} (debug build)", t.elapsed());
    }
}
