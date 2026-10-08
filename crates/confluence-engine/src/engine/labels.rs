//! Custom names users give devices and channels. Like colours, a name is
//! kept under a key that survives a restart: the device's colour key (its
//! position, for a device in one), plus `/in/<n>` or `/out/<n>` (1-based)
//! for a channel.

use std::collections::BTreeMap;

use confluence_api::{clean_label, Command};

/// Most names kept: every channel of many devices, with room to spare.
const MAX_LABELS: usize = 8192;
/// Longest key accepted.
const MAX_KEY: usize = 600;

#[derive(Default)]
pub struct Labels(BTreeMap<String, String>);

impl Labels {
    /// Sets (or with `None` or a blank name clears) the name kept under `key`.
    pub fn set(&mut self, key: &str, name: Option<&str>) -> Result<(), String> {
        if key.trim().is_empty() || key.len() > MAX_KEY {
            return Err(format!("a name key is 1 to {MAX_KEY} bytes"));
        }
        match clean_label(name) {
            Some(n) => {
                if self.0.len() >= MAX_LABELS && !self.0.contains_key(key) {
                    return Err(format!("at most {MAX_LABELS} names are kept"));
                }
                self.0.insert(key.to_string(), n);
            }
            None => {
                self.0.remove(key);
            }
        }
        Ok(())
    }

    /// The name kept under `key`, if any.
    pub fn of(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// The commands that recreate every name (for the journal).
    pub fn commands(&self) -> Vec<Command> {
        self.0.iter().map(|(key, n)| Command::SetLabel { key: key.clone(), name: Some(n.clone()) }).collect()
    }
}

/// The key of channel `index` (0-based) of the device kept under `base`.
pub fn channel_key(base: &str, input: bool, index: u32) -> String {
    format!("{base}/{}/{}", if input { "in" } else { "out" }, index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_and_blank_clears() {
        let mut l = Labels::default();
        l.set("pos:asio:1", Some("  Desk ")).unwrap();
        assert_eq!(l.of("pos:asio:1"), Some("Desk"));
        l.set("pos:asio:1", Some("  ")).unwrap();
        assert_eq!(l.of("pos:asio:1"), None);
        assert!(l.set("", Some("x")).is_err());
        assert_eq!(channel_key("pos:asio:1", false, 0), "pos:asio:1/out/1");
    }
}
