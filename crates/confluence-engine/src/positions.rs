//! The fixed position table (spec §2): virtual positions' on/off and shape,
//! and the master flag. Hardware positions are recorded with their bindings
//! in the device manager.
use std::collections::BTreeMap;

use confluence_api::{PosGroup, PosId};
use serde::{Deserialize, Serialize};

pub const SHAPES: [u32; 5] = [2, 4, 8, 16, 32];
const VAIO_SHAPE: (u32, u32) = (2, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualState {
    pub on: bool,
    pub shape: (u32, u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionTable {
    virtuals: BTreeMap<PosId, VirtualState>,
    master: Option<PosId>,
}

impl PositionTable {
    pub fn new_default() -> PositionTable {
        let mut virtuals = BTreeMap::new();
        for i in 0..PosGroup::Vasio.capacity() {
            virtuals.insert(PosId { group: PosGroup::Vasio, index: i }, VirtualState { on: i == 0, shape: (8, 8) });
        }
        virtuals.insert(PosId { group: PosGroup::Vaio, index: 0 }, VirtualState { on: false, shape: VAIO_SHAPE });
        PositionTable { virtuals, master: None }
    }
    pub fn virtual_state(&self, pos: PosId) -> Option<VirtualState> {
        self.virtuals.get(&pos).copied()
    }
    pub fn virtuals(&self) -> impl Iterator<Item = (PosId, VirtualState)> + '_ {
        self.virtuals.iter().map(|(p, v)| (*p, *v))
    }
    pub fn set_virtual(&mut self, pos: PosId, on: bool, shape: Option<(u32, u32)>) -> Result<VirtualState, String> {
        let cur =
            self.virtuals.get(&pos).copied().ok_or_else(|| format!("{} is not a virtual position", pos.label()))?;
        let shape = match pos.group {
            PosGroup::Vaio => VAIO_SHAPE,
            _ => {
                let s = shape.unwrap_or(cur.shape);
                if !SHAPES.contains(&s.0) || !SHAPES.contains(&s.1) {
                    return Err(format!("a VASIO shape is 2, 4, 8, 16 or 32 channels each way, not {}×{}", s.0, s.1));
                }
                s
            }
        };
        let v = VirtualState { on, shape };
        self.virtuals.insert(pos, v);
        Ok(v)
    }
    pub fn master(&self) -> Option<PosId> {
        self.master
    }
    pub fn set_master(&mut self, pos: Option<PosId>) -> Result<(), String> {
        if let Some(p) = pos {
            if p.group != PosGroup::Asio {
                return Err(format!("{} cannot be the master (ASIO only)", p.label()));
            }
        }
        self.master = pos;
        Ok(())
    }
}

pub fn next_free(group: PosGroup, taken: &[PosId]) -> Option<PosId> {
    (0..group.capacity()).map(|index| PosId { group, index }).find(|p| !taken.contains(p))
}

/// Today's VASIO device name for a position and engine-side shape.
pub fn vasio_device_name(pos: PosId, shape: (u32, u32)) -> String {
    format!("{}:{}x{}", pos.index + 1, shape.1, shape.0)
}

/// `Some(instance index)` for our own VASIO drivers, by old or new name.
pub fn is_own_vasio_driver(name: &str) -> Option<u8> {
    let rest = name.strip_prefix("Confluence VASIO ")?.trim();
    match rest.as_bytes() {
        [d @ b'1'..=b'8'] => Some(d - b'1'),
        [l @ b'A'..=b'H'] => Some(l - b'A'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(s: &str) -> PosId {
        s.parse().unwrap()
    }

    #[test]
    fn a_fresh_table_has_vasio_a_on_and_the_rest_off() {
        let t = PositionTable::new_default();
        assert_eq!(t.virtual_state(p("vasio:A")), Some(VirtualState { on: true, shape: (8, 8) }));
        for l in ["B", "C", "D", "E", "F", "G", "H"] {
            assert!(!t.virtual_state(p(&format!("vasio:{l}"))).unwrap().on);
        }
        assert_eq!(t.virtual_state(p("vaio:A")), Some(VirtualState { on: false, shape: (2, 0) }));
        assert_eq!(t.virtual_state(p("asio:1")), None);
    }

    #[test]
    fn virtual_shapes_and_the_master_are_checked() {
        let mut t = PositionTable::new_default();
        assert_eq!(
            t.set_virtual(p("vasio:B"), true, Some((16, 4))).unwrap(),
            VirtualState { on: true, shape: (16, 4) }
        );
        assert!(t.set_virtual(p("vasio:B"), true, Some((3, 8))).is_err(), "3 is not a shape");
        assert!(t.set_virtual(p("asio:1"), true, None).is_err(), "not virtual");
        assert_eq!(t.set_virtual(p("vaio:A"), true, Some((8, 8))).unwrap().shape, (2, 0), "VAIO's shape is fixed");
        assert!(t.set_master(Some(p("win-out:1"))).is_err());
        t.set_master(Some(p("asio:2"))).unwrap();
        assert_eq!(t.master(), Some(p("asio:2")));
    }

    #[test]
    fn names_free_positions_and_our_own_drivers() {
        assert_eq!(next_free(PosGroup::Asio, &[p("asio:1"), p("asio:3")]), Some(p("asio:2")));
        let all_app: Vec<PosId> = (0..4).map(|i| PosId { group: PosGroup::App, index: i }).collect();
        assert_eq!(next_free(PosGroup::App, &all_app), None);
        assert_eq!(vasio_device_name(p("vasio:C"), (8, 2)), "3:2x8");
        assert_eq!(is_own_vasio_driver("Confluence VASIO 1"), Some(0));
        assert_eq!(is_own_vasio_driver("Confluence VASIO H"), Some(7));
        assert_eq!(is_own_vasio_driver("Confluence VASIO 9"), None);
        assert_eq!(is_own_vasio_driver("GoXLR ASIO Driver"), None);
    }
}
