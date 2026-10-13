//! The Devices screen's type bays: Hardware, Windows, Virtual, Network and
//! Apps, each a recessed zone with its own colour (spec: meter bridge §4.3),
//! packed onto the screen like tiles.

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn groups_map_to_their_bays() {
        assert_eq!(bay_of(PosGroup::WinIn), Bay::Windows);
        assert_eq!(bay_of(PosGroup::WinOut), Bay::Windows);
        assert_eq!(bay_of(PosGroup::Vaio), Bay::Vaio);
        assert_eq!(bay_of(PosGroup::NetOut), Bay::Network);
        for b in Bay::all() {
            assert!(b.groups().iter().all(|g| bay_of(*g) == b));
        }
    }
}
