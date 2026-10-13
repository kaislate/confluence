//! The Devices screen's type bays: Hardware, Windows, Virtual, Network and
//! Apps, each a recessed zone with its own colour (spec: meter bridge §4.3),
//! packed onto the screen like tiles.

use std::collections::HashSet;

use confluence_api::{PosGroup, PositionState, PositionStatus};
use eframe::egui::Color32;

/// A type of audio, and the bay its devices live in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bay {
    Hardware,
    Windows,
    Virtual,
    Vaio,
    Network,
    Apps,
}

impl Bay {
    pub fn all() -> [Bay; 6] {
        [Bay::Hardware, Bay::Windows, Bay::Virtual, Bay::Vaio, Bay::Network, Bay::Apps]
    }

    pub fn title(self) -> &'static str {
        match self {
            Bay::Hardware => "Hardware",
            Bay::Windows => "Windows",
            Bay::Virtual => "Virtual",
            Bay::Vaio => "VAIO",
            Bay::Network => "Network",
            Bay::Apps => "Apps",
        }
    }

    /// The bay's descriptive title (the default; one word with "Short bay titles").
    pub fn description(self) -> &'static str {
        match self {
            Bay::Hardware => "Audio interfaces",
            Bay::Windows => "Windows playback & recording",
            Bay::Virtual => "Virtual devices for DAWs",
            Bay::Vaio => "Virtual Windows devices",
            Bay::Network => "Network streams",
            Bay::Apps => "Captured apps",
        }
    }

    /// The bay's colour: its strip, label and its devices' names on the bridge.
    pub fn color(self) -> Color32 {
        match self {
            Bay::Hardware => Color32::from_rgb(0xe9, 0xa5, 0x4b),
            Bay::Windows => Color32::from_rgb(0x55, 0xc7, 0xe9),
            Bay::Virtual => Color32::from_rgb(0x6a, 0xa7, 0xff),
            Bay::Vaio => Color32::from_rgb(0x8e, 0x9c, 0xff),
            Bay::Network => Color32::from_rgb(0x55, 0xd6, 0xa0),
            Bay::Apps => Color32::from_rgb(0xc4, 0x8c, 0xff),
        }
    }

    /// The position groups in this bay, in order.
    pub fn groups(self) -> &'static [PosGroup] {
        match self {
            Bay::Hardware => &[PosGroup::Asio],
            Bay::Windows => &[PosGroup::WinIn, PosGroup::WinOut],
            Bay::Virtual => &[PosGroup::Vasio],
            Bay::Vaio => &[PosGroup::Vaio],
            Bay::Network => &[PosGroup::NetIn, PosGroup::NetOut],
            Bay::Apps => &[PosGroup::App],
        }
    }
}

/// The bay a position group lives in.
pub fn bay_of(g: PosGroup) -> Bay {
    match g {
        PosGroup::Asio => Bay::Hardware,
        PosGroup::WinIn | PosGroup::WinOut => Bay::Windows,
        PosGroup::Vasio => Bay::Virtual,
        PosGroup::Vaio => Bay::Vaio,
        PosGroup::NetIn | PosGroup::NetOut => Bay::Network,
        PosGroup::App => Bay::Apps,
    }
}

/// A position that holds nothing the user chose: an empty tray or a
/// switched-off virtual position.
pub fn vacant(p: &PositionState) -> bool {
    matches!(p.status, PositionStatus::Empty | PositionStatus::Off)
}

/// One bay as shown: its devices (in the user's order), then the next free
/// position of each of its groups (or every position when expanded), and
/// how many stay folded away.
pub struct BayView<'a> {
    pub bay: Bay,
    pub cards: Vec<&'a PositionState>,
    pub hidden: usize,
    /// The first `movable` cards hold devices: they can be moved and folded.
    pub movable: usize,
}

/// The bays in screen order. A bay with no positions at all is left out.
pub fn bay_views<'a>(
    positions: &'a [PositionState],
    expanded: &HashSet<Bay>,
    layout: &crate::prefs::LayoutPrefs,
) -> Vec<BayView<'a>> {
    Bay::all()
        .into_iter()
        .filter_map(|bay| {
            let mut cards = Vec::new();
            let mut vacants = Vec::new();
            let mut hidden = 0;
            for &g in bay.groups() {
                let mut of: Vec<&PositionState> = positions.iter().filter(|p| p.pos.group == g).collect();
                of.sort_by_key(|p| p.pos.index);
                cards.extend(of.iter().copied().filter(|p| !vacant(p)));
                let free: Vec<&PositionState> = of.iter().copied().filter(|p| vacant(p)).collect();
                if expanded.contains(&bay) {
                    vacants.extend(free);
                } else {
                    hidden += free.len().saturating_sub(1);
                    vacants.extend(free.first().copied());
                }
            }
            // The devices in the user's order; new ones after, in position order.
            let default: Vec<String> = cards.iter().map(|p| p.pos.to_string()).collect();
            let order = crate::arrange::ordered(&default, layout.cards.get(bay.title()).map_or(&[], |v| v.as_slice()));
            cards.sort_by_key(|p| order.iter().position(|k| *k == p.pos.to_string()));
            let movable = cards.len();
            cards.extend(vacants);
            (!cards.is_empty()).then_some(BayView { bay, cards, hidden, movable })
        })
        .collect()
}

/// Each row's height: its tallest card's (`places` from [`place_cards`]).
pub fn row_heights(places: &[(usize, usize)], heights: &[f32]) -> Vec<f32> {
    let rows = places.iter().map(|p| p.0 + 1).max().unwrap_or(0);
    let mut out = vec![0.0f32; rows];
    for (&(r, _), &h) in places.iter().zip(heights) {
        out[r] = out[r].max(h);
    }
    out
}

/// Packs bays of `widths` into rows no wider than `avail` (shelf packing in
/// order): a bay that does not fit on the current row starts the next one;
/// a bay wider than `avail` gets a row to itself.
pub fn pack_bays(widths: &[f32], avail: f32, gap: f32) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = 0.0;
    for (i, &w) in widths.iter().enumerate() {
        match rows.last_mut() {
            Some(row) if used + gap + w <= avail + 0.01 => {
                row.push(i);
                used += gap + w;
            }
            _ => {
                rows.push(vec![i]);
                used = w;
            }
        }
    }
    rows
}

/// Where cards of `spans` columns go in a bay at most `max_cols` wide: each
/// card's (row, column), in order, a card that does not fit the rest of a
/// row starting the next; then the columns used and the rows. Spans wider
/// than the bay are clamped to it.
pub fn place_cards(spans: &[usize], max_cols: usize) -> (Vec<(usize, usize)>, usize, usize) {
    let max_cols = max_cols.max(1);
    let (mut row, mut col, mut used) = (0usize, 0usize, 0usize);
    let mut at = Vec::with_capacity(spans.len());
    for &s in spans {
        let s = s.clamp(1, max_cols);
        if col > 0 && col + s > max_cols {
            row += 1;
            col = 0;
        }
        at.push((row, col));
        col += s;
        used = used.max(col);
    }
    (at, used.max(1), row + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::{all_positions, PositionDevice};

    #[test]
    fn vaio_has_its_own_bay_after_virtual() {
        assert_eq!(bay_of(PosGroup::Vaio), Bay::Vaio);
        assert_eq!(bay_of(PosGroup::Vasio), Bay::Virtual);
        assert_eq!(Bay::Virtual.groups(), &[PosGroup::Vasio]);
        let all = Bay::all();
        let i = all.iter().position(|b| *b == Bay::Virtual).unwrap();
        assert_eq!(all[i + 1], Bay::Vaio);
        assert_eq!(Bay::Vaio.title(), "VAIO");
        assert_ne!(Bay::Vaio.color(), Bay::Virtual.color());
    }

    #[test]
    fn a_four_column_card_in_a_three_column_bay_is_clamped() {
        assert_eq!(place_cards(&[4, 1], 3), (vec![(0, 0), (1, 0)], 3, 2));
        assert_eq!(place_cards(&[1, 3], 4), (vec![(0, 0), (0, 1)], 4, 1));
    }

    #[test]
    fn cards_with_spans_pack_row_by_row() {
        assert_eq!(place_cards(&[2, 1, 1, 1], 4), (vec![(0, 0), (0, 2), (0, 3), (1, 0)], 4, 2));
        assert_eq!(place_cards(&[1, 2, 2], 3), (vec![(0, 0), (0, 1), (1, 0)], 3, 2));
        assert_eq!(place_cards(&[2], 1), (vec![(0, 0)], 1, 1), "a double card in a one-column bay");
        assert_eq!(place_cards(&[1, 1], 4), (vec![(0, 0), (0, 1)], 2, 1), "a bay is as wide as its cards");
        assert_eq!(place_cards(&[], 4), (vec![], 1, 1));
    }

    fn st(pos: &str, status: PositionStatus) -> PositionState {
        PositionState {
            pos: pos.parse().unwrap(),
            status,
            device: Some(PositionDevice { kind: confluence_api::DeviceKind::Asio, name: "x".into() }),
            shape: None,
            daw: None,
            master: false,
            color: None,
            slots: vec![],
        }
    }

    #[test]
    fn a_bays_cards_follow_the_saved_order_and_free_ones_stay_last() {
        let mut ps: Vec<PositionState> =
            all_positions().into_iter().map(|p| st(&p.to_string(), PositionStatus::Empty)).collect();
        for p in ["win-out:1", "win-out:2", "win-in:1"] {
            ps.iter_mut().find(|q| q.pos.to_string() == p).unwrap().status = PositionStatus::Filled { online: true };
        }
        let mut lp = crate::prefs::LayoutPrefs::default();
        lp.cards.insert("Windows".into(), vec!["win-out:2".into(), "gone:1".into(), "win-in:1".into()]);
        let names = |views: &[BayView]| -> Vec<String> {
            views.iter().find(|v| v.bay == Bay::Windows).unwrap().cards.iter().map(|p| p.pos.to_string()).collect()
        };
        let views = bay_views(&ps, &HashSet::new(), &lp);
        assert_eq!(names(&views), vec!["win-out:2", "win-in:1", "win-out:1", "win-in:2", "win-out:3"]);
        let all = bay_views(&ps, &[Bay::Windows].into_iter().collect(), &lp);
        let shown = names(&all);
        assert_eq!(&shown[..3], &["win-out:2", "win-in:1", "win-out:1"], "expanded: devices first, in order");
        assert_eq!(shown.len(), 16);
    }

    #[test]
    fn a_row_is_as_tall_as_its_tallest_card() {
        let (places, _, rows) = place_cards(&[1, 1, 1, 1], 3);
        assert_eq!(rows, 2);
        assert_eq!(row_heights(&places, &[46.0, 150.0, 46.0, 46.0]), vec![150.0, 46.0]);
        assert_eq!(row_heights(&[], &[]), Vec::<f32>::new());
    }

    #[test]
    fn bays_pack_like_tiles_in_order() {
        assert_eq!(
            pack_bays(&[500.0, 260.0, 1300.0, 260.0, 260.0], 1500.0, 20.0),
            vec![vec![0, 1], vec![2], vec![3, 4]]
        );
        assert_eq!(
            pack_bays(&[500.0, 260.0, 1300.0, 260.0, 260.0], 1700.0, 20.0),
            vec![vec![0, 1], vec![2, 3], vec![4]]
        );
        assert_eq!(pack_bays(&[2000.0, 100.0], 1700.0, 20.0), vec![vec![0], vec![1]], "too wide: a row of its own");
        assert!(pack_bays(&[], 1000.0, 20.0).is_empty());
    }

    #[test]
    fn groups_map_to_their_bays() {
        assert_eq!(bay_of(PosGroup::WinIn), Bay::Windows);
        assert_eq!(bay_of(PosGroup::WinOut), Bay::Windows);
        assert_eq!(bay_of(PosGroup::Vaio), Bay::Vaio);
        assert_eq!(bay_of(PosGroup::NetOut), Bay::Network);
        for b in Bay::all() {
            assert!(b.groups().iter().all(|g| bay_of(*g) == b));
        }
    }

    #[test]
    fn a_bay_shows_its_devices_and_each_groups_next_free_position() {
        let mut ps: Vec<PositionState> =
            all_positions().into_iter().map(|p| st(&p.to_string(), PositionStatus::Empty)).collect();
        for p in ps.iter_mut().filter(|p| p.pos.group == PosGroup::Vasio || p.pos.group == PosGroup::Vaio) {
            p.status = PositionStatus::Off;
        }
        ps.iter_mut().find(|p| p.pos.to_string() == "win-in:1").unwrap().status =
            PositionStatus::Filled { online: true };
        ps.iter_mut().find(|p| p.pos.to_string() == "vasio:A").unwrap().status = PositionStatus::On { online: false };
        let views = bay_views(&ps, &HashSet::new(), &crate::prefs::LayoutPrefs::default());
        let names = |b: Bay| -> Vec<String> {
            views.iter().find(|v| v.bay == b).unwrap().cards.iter().map(|p| p.pos.to_string()).collect()
        };
        assert_eq!(names(Bay::Windows), vec!["win-in:1", "win-in:2", "win-out:1"]);
        assert_eq!(names(Bay::Virtual), vec!["vasio:A", "vasio:B"]);
        assert_eq!(names(Bay::Vaio), vec!["vaio:A"]);
        assert_eq!(names(Bay::Hardware), vec!["asio:1"]);
        let windows = views.iter().find(|v| v.bay == Bay::Windows).unwrap();
        assert_eq!(windows.hidden, 6 + 7, "WIN IN 3-8 and WIN OUT 2-8 are folded");
        let all = bay_views(&ps, &[Bay::Virtual].into_iter().collect(), &crate::prefs::LayoutPrefs::default());
        assert_eq!(all.iter().find(|v| v.bay == Bay::Virtual).unwrap().cards.len(), 8);
    }
}
