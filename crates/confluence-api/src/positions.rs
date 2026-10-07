//! Fixed slot positions (spec §2): what a position is called and what it holds.
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{DeviceKind, Rgb};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PosGroup {
    Vasio,
    Vaio,
    Asio,
    WinIn,
    WinOut,
    App,
    NetIn,
    NetOut,
}

impl PosGroup {
    pub fn all() -> [PosGroup; 8] {
        use PosGroup::*;
        [Vasio, Vaio, Asio, WinIn, WinOut, App, NetIn, NetOut]
    }
    pub fn capacity(self) -> u8 {
        match self {
            PosGroup::Vasio | PosGroup::Asio | PosGroup::WinIn | PosGroup::WinOut => 8,
            PosGroup::Vaio => 1,
            _ => 4,
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            PosGroup::Vasio => "vasio",
            PosGroup::Vaio => "vaio",
            PosGroup::Asio => "asio",
            PosGroup::WinIn => "win-in",
            PosGroup::WinOut => "win-out",
            PosGroup::App => "app",
            PosGroup::NetIn => "net-in",
            PosGroup::NetOut => "net-out",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            PosGroup::Vasio => "VASIO",
            PosGroup::Vaio => "VAIO",
            PosGroup::Asio => "ASIO",
            PosGroup::WinIn => "WIN IN",
            PosGroup::WinOut => "WIN OUT",
            PosGroup::App => "APP",
            PosGroup::NetIn => "NET IN",
            PosGroup::NetOut => "NET OUT",
        }
    }
    pub fn is_virtual(self) -> bool {
        matches!(self, PosGroup::Vasio | PosGroup::Vaio)
    }
    /// Virtual positions are lettered (A, B, …), hardware ones numbered from 1.
    fn lettered(self) -> bool {
        self.is_virtual()
    }
    pub fn kinds(self) -> &'static [DeviceKind] {
        match self {
            PosGroup::Vasio => &[DeviceKind::Vasio],
            PosGroup::Vaio => &[DeviceKind::Vaio],
            PosGroup::Asio => &[DeviceKind::Asio],
            PosGroup::WinIn => &[DeviceKind::WasapiCapture],
            PosGroup::WinOut => &[DeviceKind::WasapiRender],
            PosGroup::App => &[DeviceKind::AppCapture],
            PosGroup::NetIn => &[DeviceKind::NetReceive],
            PosGroup::NetOut => &[DeviceKind::NetSend],
        }
    }
    pub fn for_kind(k: DeviceKind) -> PosGroup {
        match k {
            DeviceKind::Vasio => PosGroup::Vasio,
            DeviceKind::Vaio => PosGroup::Vaio,
            DeviceKind::Asio => PosGroup::Asio,
            DeviceKind::WasapiCapture => PosGroup::WinIn,
            DeviceKind::WasapiRender => PosGroup::WinOut,
            DeviceKind::AppCapture => PosGroup::App,
            DeviceKind::NetReceive => PosGroup::NetIn,
            DeviceKind::NetSend => PosGroup::NetOut,
        }
    }
}

/// A position: its group and 0-based index (shown as A… or 1…).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PosId {
    pub group: PosGroup,
    pub index: u8,
}

impl PosId {
    /// What a card shows: "VASIO A", "ASIO 3".
    pub fn label(&self) -> String {
        format!("{} {}", self.group.label(), self.suffix())
    }
    fn suffix(&self) -> String {
        if self.group.lettered() {
            ((b'A' + self.index) as char).to_string()
        } else {
            (self.index + 1).to_string()
        }
    }
}

impl fmt::Display for PosId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.group.prefix(), self.suffix())
    }
}

impl FromStr for PosId {
    type Err = String;
    fn from_str(s: &str) -> Result<PosId, String> {
        let bad = || format!("{s:?} is not a position (e.g. asio:3, vasio:A)");
        let (prefix, rest) = s.trim().split_once(':').ok_or_else(bad)?;
        let group = PosGroup::all().into_iter().find(|g| g.prefix().eq_ignore_ascii_case(prefix)).ok_or_else(bad)?;
        let index = if group.lettered() {
            let c = rest.trim().to_ascii_uppercase();
            let b = c.as_bytes();
            if b.len() != 1 || !b[0].is_ascii_uppercase() {
                return Err(bad());
            }
            b[0] - b'A'
        } else {
            let n: u8 = rest.trim().parse().map_err(|_| bad())?;
            if n == 0 {
                return Err(bad());
            }
            n - 1
        };
        if index >= group.capacity() {
            return Err(bad());
        }
        Ok(PosId { group, index })
    }
}

impl Serialize for PosId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for PosId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<PosId, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

pub fn all_positions() -> Vec<PosId> {
    PosGroup::all().into_iter().flat_map(|g| (0..g.capacity()).map(move |index| PosId { group: g, index })).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionStatus {
    Empty,
    Filled { online: bool },
    Off,
    On { online: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionDevice {
    pub kind: DeviceKind,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionState {
    pub pos: PosId,
    pub status: PositionStatus,
    pub device: Option<PositionDevice>,
    /// A virtual position's (inputs, outputs) as the engine sees them.
    pub shape: Option<(u32, u32)>,
    /// The program using a VASIO position, if it said.
    pub daw: Option<String>,
    pub master: bool,
    pub color: Option<Rgb>,
    /// The engine slots this position owns.
    pub slots: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceKind;

    #[test]
    fn position_ids_round_trip_through_their_names() {
        for p in all_positions() {
            assert_eq!(p.to_string().parse::<PosId>().unwrap(), p, "{p}");
        }
        assert_eq!("vasio:A".parse::<PosId>().unwrap(), PosId { group: PosGroup::Vasio, index: 0 });
        assert_eq!("asio:3".parse::<PosId>().unwrap(), PosId { group: PosGroup::Asio, index: 2 });
        assert!("asio:9".parse::<PosId>().is_err(), "past the group's capacity");
        assert!("vasio:I".parse::<PosId>().is_err());
        assert!("asio:0".parse::<PosId>().is_err());
        assert!("bogus:1".parse::<PosId>().is_err());
        assert_eq!(all_positions().len(), 8 + 1 + 8 + 8 + 8 + 4 + 4 + 4);
    }

    #[test]
    fn every_device_kind_has_one_group() {
        for k in [
            DeviceKind::Asio,
            DeviceKind::WasapiRender,
            DeviceKind::WasapiCapture,
            DeviceKind::AppCapture,
            DeviceKind::Vasio,
            DeviceKind::Vaio,
            DeviceKind::NetSend,
            DeviceKind::NetReceive,
        ] {
            assert!(PosGroup::for_kind(k).kinds().contains(&k), "{k:?}");
        }
        assert!(PosGroup::Vasio.is_virtual() && PosGroup::Vaio.is_virtual() && !PosGroup::Asio.is_virtual());
    }
}
