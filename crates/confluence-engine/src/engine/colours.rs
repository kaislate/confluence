//! Colours users give devices. A colour is kept under a key that survives a
//! restart (slot ids do not): the slot's device, which all its slots share,
//! or for an insert bus its first send column (as scenes name buses).

use std::collections::BTreeMap;

use confluence_api::{Command, Rgb, SlotState};

/// Most colours kept: one per device or bus, with room to spare.
const MAX_COLOURS: usize = 1024;
/// Longest key accepted (device strings are far shorter).
const MAX_KEY: usize = 512;

/// The key slot `s`'s colour is kept under.
pub fn key(s: &SlotState) -> String {
    if s.is_bus() {
        format!("bus:{}", s.first_output)
    } else if s.device.is_empty() {
        format!("slot:{}", s.name)
    } else {
        s.device.clone()
    }
}

#[derive(Default)]
pub struct Colours(BTreeMap<String, Rgb>);

impl Colours {
    /// Sets (or with `None` clears) the colour kept under `key`.
    pub fn set(&mut self, key: &str, color: Option<Rgb>) -> Result<(), String> {
        if key.trim().is_empty() || key.len() > MAX_KEY {
            return Err("a colour key is 1 to 512 bytes".into());
        }
        match color {
            Some(c) => {
                if self.0.len() >= MAX_COLOURS && !self.0.contains_key(key) {
                    return Err(format!("at most {MAX_COLOURS} colours are kept"));
                }
                self.0.insert(key.to_string(), c);
            }
            None => {
                self.0.remove(key);
            }
        }
        Ok(())
    }

    /// The colour chosen for slot `s`, if any.
    pub fn of(&self, s: &SlotState) -> Option<Rgb> {
        self.0.get(&key(s)).copied()
    }

    /// The commands that recreate every colour (for the journal).
    pub fn commands(&self) -> Vec<Command> {
        self.0.iter().map(|(key, c)| Command::SetColor { key: key.clone(), color: Some(*c) }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_checked_and_the_number_of_colours_is_bounded() {
        let mut c = Colours::default();
        assert!(c.set("", Some([1, 1, 1])).is_err());
        assert!(c.set(&"x".repeat(MAX_KEY + 1), Some([1, 1, 1])).is_err());
        for i in 0..MAX_COLOURS {
            c.set(&format!("dev:{i}"), Some([1, 1, 1])).unwrap();
        }
        assert!(c.set("one more", Some([1, 1, 1])).is_err());
        assert!(c.set("dev:0", Some([2, 2, 2])).is_ok(), "changing a kept colour is fine");
        assert!(c.set("one more", None).is_ok(), "clearing is always fine");
    }
}
