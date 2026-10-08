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
    Network,
    Apps,
}

impl Bay {
    pub fn all() -> [Bay; 5] {
        [Bay::Hardware, Bay::Windows, Bay::Virtual, Bay::Network, Bay::Apps]
    }

    pub fn title(self) -> &'static str {
        match self {
            Bay::Hardware => "Hardware",
            Bay::Windows => "Windows",
            Bay::Virtual => "Virtual",
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
            Bay::Network => Color32::from_rgb(0x55, 0xd6, 0xa0),
            Bay::Apps => Color32::from_rgb(0xc4, 0x8c, 0xff),
        }
    }

    /// The position groups in this bay, in order.
    pub fn groups(self) -> &'static [PosGroup] {
        match self {
            Bay::Hardware => &[PosGroup::Asio],
            Bay::Windows => &[PosGroup::WinIn, PosGroup::WinOut],
            Bay::Virtual => &[PosGroup::Vasio, PosGroup::Vaio],
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
        PosGroup::Vasio | PosGroup::Vaio => Bay::Virtual,
        PosGroup::NetIn | PosGroup::NetOut => Bay::Network,
        PosGroup::App => Bay::Apps,
    }
}

/// A position that holds nothing the user chose: an empty tray or a
/// switched-off virtual position.
pub fn vacant(p: &PositionState) -> bool {
    matches!(p.status, PositionStatus::Empty | PositionStatus::Off)
}

/// One bay as shown: its devices, then the next free position of each of
/// its groups (or every position when expanded), and how many stay folded.
pub struct BayView<'a> {
    pub bay: Bay,
    pub cards: Vec<&'a PositionState>,
    pub hidden: usize,
}

/// The bays in screen order. A bay with no positions at all is left out.
pub fn bay_views<'a>(positions: &'a [PositionState], expanded: &HashSet<Bay>) -> Vec<BayView<'a>> {
    Bay::all()
        .into_iter()
        .filter_map(|bay| {
            let mut cards = Vec::new();
            let mut vacants = Vec::new();
            let mut hidden = 0;
            for &g in bay.groups() {
                let mut of: Vec<&PositionState> = positions.iter().filter(|p| p.pos.group == g).collect();
                of.sort_by_key(|p| p.pos.index);
                if expanded.contains(&bay) {
                    // Switched-on virtual positions first, then the rest in order.
                    of.sort_by_key(|p| vacant(p));
                    cards.extend(of);
                    continue;
                }
                cards.extend(of.iter().copied().filter(|p| !vacant(p)));
                let free: Vec<&PositionState> = of.iter().copied().filter(|p| vacant(p)).collect();
                hidden += free.len().saturating_sub(1);
                vacants.extend(free.first().copied());
            }
            cards.extend(vacants);
            (!cards.is_empty()).then_some(BayView { bay, cards, hidden })
        })
        .collect()
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
        assert_eq!(bay_of(PosGroup::Vaio), Bay::Virtual);
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
        let views = bay_views(&ps, &HashSet::new());
        let names = |b: Bay| -> Vec<String> {
            views.iter().find(|v| v.bay == b).unwrap().cards.iter().map(|p| p.pos.to_string()).collect()
        };
        assert_eq!(names(Bay::Windows), vec!["win-in:1", "win-in:2", "win-out:1"]);
        assert_eq!(names(Bay::Virtual), vec!["vasio:A", "vasio:B", "vaio:A"]);
        assert_eq!(names(Bay::Hardware), vec!["asio:1"]);
        let windows = views.iter().find(|v| v.bay == Bay::Windows).unwrap();
        assert_eq!(windows.hidden, 6 + 7, "WIN IN 3-8 and WIN OUT 2-8 are folded");
        let all = bay_views(&ps, &[Bay::Virtual].into_iter().collect());
        assert_eq!(all.iter().find(|v| v.bay == Bay::Virtual).unwrap().cards.len(), 9);
    }
}
