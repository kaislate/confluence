//! Arranging devices on a screen: the saved order, moves, drop points while
//! dragging, and how much of a folded crossing is routed. Pure, so it can be
//! tested without a window.

use std::collections::BTreeSet;
use std::ops::Range;

use eframe::egui::{Pos2, Rect};

/// The keys in screen order: the saved keys still present, in saved order,
/// then the others in their default order.
pub fn ordered(default: &[String], saved: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(default.len());
    for k in saved.iter().chain(default) {
        if default.contains(k) && !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

/// `order` with `key` moved to insertion point `to` (an index into `order`
/// as it is, before the key leaves its place; clamped to the end).
pub fn moved(order: &[String], key: &str, to: usize) -> Vec<String> {
    let Some(from) = order.iter().position(|k| k == key) else { return order.to_vec() };
    let mut out = order.to_vec();
    let k = out.remove(from);
    let to = to.min(order.len());
    // Past its old place, the insertion point moves up by the key that left.
    let to = if to > from { to - 1 } else { to };
    out.insert(to, k);
    out
}

/// Adds `key` to `set`, or takes it out if it is there.
pub fn toggle(set: &mut BTreeSet<String>, key: &str) {
    if !set.remove(key) {
        set.insert(key.to_string());
    }
}

/// Where a band dropped at offset `p` lands: the number of band midpoints
/// (`mids`, in order) before it.
pub fn drop_slot(mids: &[f32], p: f32) -> usize {
    mids.iter().filter(|&&m| m < p).count()
}

/// Where a card dropped at `at` lands among cards at `rects` (in order):
/// before the nearest card when left of its centre, else after it.
pub fn drop_index(rects: &[Rect], at: Pos2) -> usize {
    // Below every card: the end.
    if rects.iter().all(|r| at.y > r.bottom()) {
        return rects.len();
    }
    let nearest = rects
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.distance_sq_to_pos(at).total_cmp(&b.1.distance_sq_to_pos(at)))
        .map(|(i, _)| i);
    match nearest {
        Some(i) if at.x < rects[i].center().x => i,
        Some(i) => i + 1,
        None => 0,
    }
}

/// How much of a crossing is routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    None,
    Partial,
    Full,
}

/// The coverage of inputs `ins` by outputs `outs`: full when every input
/// reaches one of the outputs and every output hears one of the inputs (a
/// stereo pair routed 1:1 is full), none without any route, else partial.
pub fn coverage(ins: Range<u32>, outs: Range<u32>, routed: impl Fn(u32, u32) -> bool) -> Coverage {
    let mut any = false;
    let mut heard = vec![false; outs.len()];
    let mut every_input = true;
    for i in ins.clone() {
        let mut reaches = false;
        for (k, o) in outs.clone().enumerate() {
            if routed(i, o) {
                reaches = true;
                heard[k] = true;
            }
        }
        any |= reaches;
        every_input &= reaches;
    }
    if !any {
        Coverage::None
    } else if every_input && heard.iter().all(|h| *h) {
        Coverage::Full
    } else {
        Coverage::Partial
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{pos2, vec2};

    fn keys(s: &[&str]) -> Vec<String> {
        s.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn saved_keys_lead_and_new_ones_follow_in_default_order() {
        let default = keys(&["a", "b", "c", "d"]);
        assert_eq!(ordered(&default, &[]), default, "nothing saved: the default order");
        assert_eq!(ordered(&default, &keys(&["c", "a"])), keys(&["c", "a", "b", "d"]));
        assert_eq!(
            ordered(&default, &keys(&["gone", "d", "d", "b"])),
            keys(&["d", "b", "a", "c"]),
            "gone and repeated keys"
        );
        assert!(ordered(&[], &keys(&["a"])).is_empty());
    }

    #[test]
    fn moving_a_key_puts_it_at_the_drop_point() {
        let o = keys(&["a", "b", "c", "d"]);
        assert_eq!(moved(&o, "c", 0), keys(&["c", "a", "b", "d"]), "to the start");
        assert_eq!(moved(&o, "a", 4), keys(&["b", "c", "d", "a"]), "to the end");
        assert_eq!(moved(&o, "a", 99), keys(&["b", "c", "d", "a"]), "clamped");
        assert_eq!(moved(&o, "b", 1), o, "onto its own place");
        assert_eq!(moved(&o, "b", 2), o, "just after itself");
        assert_eq!(moved(&o, "b", 3), keys(&["a", "c", "b", "d"]), "down by one");
        assert_eq!(moved(&o, "x", 0), o, "an unknown key changes nothing");
    }

    #[test]
    fn toggling_adds_then_removes() {
        let mut s = BTreeSet::new();
        toggle(&mut s, "a");
        assert!(s.contains("a"));
        toggle(&mut s, "a");
        assert!(s.is_empty());
    }

    #[test]
    fn a_drop_lands_between_the_midpoints() {
        let mids = [10.0, 50.0, 90.0];
        assert_eq!(drop_slot(&mids, -5.0), 0);
        assert_eq!(drop_slot(&mids, 30.0), 1);
        assert_eq!(drop_slot(&mids, 70.0), 2);
        assert_eq!(drop_slot(&mids, 200.0), 3);
        assert_eq!(drop_slot(&[], 3.0), 0);
    }

    #[test]
    fn a_card_drop_goes_before_or_after_the_nearest_card() {
        let card = |x: f32, y: f32| Rect::from_min_size(pos2(x, y), vec2(100.0, 50.0));
        // Two rows: [0][1][2] / [3]
        let rects = [card(0.0, 0.0), card(110.0, 0.0), card(220.0, 0.0), card(0.0, 60.0)];
        assert_eq!(drop_index(&rects, pos2(120.0, 20.0)), 1, "left half of card 1");
        assert_eq!(drop_index(&rects, pos2(200.0, 20.0)), 2, "right half of card 1");
        assert_eq!(drop_index(&rects, pos2(400.0, 20.0)), 3, "past the row's end");
        assert_eq!(drop_index(&rects, pos2(5.0, 90.0)), 3, "left of the second row's card");
        assert_eq!(drop_index(&rects, pos2(300.0, 300.0)), 4, "below everything");
        assert_eq!(drop_index(&[], pos2(0.0, 0.0)), 0);
    }

    #[test]
    fn coverage_is_full_when_every_channel_on_both_sides_is_touched() {
        let routes = |list: &'static [(u32, u32)]| move |i, o| list.contains(&(i, o));
        assert_eq!(coverage(0..2, 10..12, routes(&[(0, 10), (1, 11)])), Coverage::Full, "stereo 1:1");
        assert_eq!(coverage(0..2, 10..12, routes(&[(0, 10)])), Coverage::Partial, "only the left");
        assert_eq!(coverage(0..2, 10..12, routes(&[(5, 10)])), Coverage::None, "routes elsewhere");
        assert_eq!(coverage(0..2, 10..11, routes(&[(0, 10), (1, 10)])), Coverage::Full, "both into one output");
        assert_eq!(coverage(0..2, 10..11, routes(&[(1, 10)])), Coverage::Partial, "one of two into one");
        assert_eq!(coverage(3..4, 10..11, routes(&[(3, 10)])), Coverage::Full, "a plain cell");
        assert_eq!(coverage(0..0, 10..11, routes(&[])), Coverage::None, "empty");
    }
}
