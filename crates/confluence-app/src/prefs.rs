//! How the user likes things shown: meter style, names, the meter bridge.
//! View preferences kept by the app (eframe storage, one JSON entry), not
//! engine state.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::gear::oled_meter::{MeterLook, MeterStyle};

/// Where the preferences are stored.
pub const PREFS_KEY: &str = "view_prefs";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewPrefs {
    pub meter: MeterLook,
    /// Show only custom names; device names are hidden (a device without
    /// one still shows its device name).
    pub only_custom_names: bool,
    pub bridge: BridgePrefs,
}

impl Default for ViewPrefs {
    fn default() -> Self {
        ViewPrefs {
            meter: MeterLook { style: MeterStyle::Segments, double_peak: false, clip_red: true },
            only_custom_names: false,
            bridge: BridgePrefs::default(),
        }
    }
}

/// The meter bridge: shown or collapsed, its height, what it hides, and its
/// pop-out window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgePrefs {
    pub shown: bool,
    /// Its height as a fraction of the screen.
    pub height_frac: f32,
    /// Devices left off the bridge, by colour key (`pos:asio:1`).
    pub hidden_devices: BTreeSet<String>,
    /// Channels left off, by `key/in/n` or `key/out/n` (1-based).
    pub hidden_channels: BTreeSet<String>,
    /// In its own window.
    pub popped: bool,
    /// That window stays on top.
    pub pinned: bool,
    /// That window's last position and size: x, y, width, height.
    pub window: Option<[f32; 4]>,
}

impl Default for BridgePrefs {
    fn default() -> Self {
        BridgePrefs {
            shown: true,
            height_frac: 0.22,
            hidden_devices: BTreeSet::new(),
            hidden_channels: BTreeSet::new(),
            popped: false,
            pinned: false,
            window: None,
        }
    }
}

impl ViewPrefs {
    pub fn to_storage(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The saved preferences; defaults where none were saved or they don't read.
    pub fn from_storage(text: Option<&str>) -> ViewPrefs {
        text.and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gear::oled_meter::MeterStyle;

    #[test]
    fn prefs_round_trip_through_storage_and_bad_text_gives_defaults() {
        let mut p = ViewPrefs::default();
        assert_eq!(p.meter.style, MeterStyle::Segments);
        assert!(p.bridge.shown && !p.bridge.popped && !p.only_custom_names);
        assert!((p.bridge.height_frac - 0.22).abs() < 1e-6);
        p.meter.style = MeterStyle::DotMatrix;
        p.meter.double_peak = true;
        p.meter.clip_red = false;
        p.only_custom_names = true;
        p.bridge.hidden_devices.insert("pos:asio:1".into());
        p.bridge.pinned = true;
        let text = p.to_storage();
        assert_eq!(ViewPrefs::from_storage(Some(&text)), p);
        assert_eq!(ViewPrefs::from_storage(None), ViewPrefs::default());
        assert_eq!(ViewPrefs::from_storage(Some("not json")), ViewPrefs::default());
        // Older or partial text keeps the defaults for what it lacks.
        let partial = ViewPrefs::from_storage(Some(r#"{"only_custom_names":true}"#));
        assert!(partial.only_custom_names);
        assert!(partial.bridge.shown);
    }
}
