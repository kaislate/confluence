//! devices.json version 1 -> 2 (spec §3): every saved device goes into a
//! position of its kind, keeping its channels exactly.
use std::path::{Path, PathBuf};

use confluence_api::{DeviceKind, PosGroup, PosId};

use crate::devices::{parse_vasio, Binding};
use crate::positions::{is_own_vasio_driver, next_free, vasio_device_name};

/// A position, its on/off and shape (virtual positions), and its device.
pub type MigratedPosition = (PosId, Option<(bool, (u32, u32))>, Option<Binding>);

pub struct SavedV2Parts {
    pub master: Option<PosId>,
    pub master_binding: Option<Binding>,
    pub positions: Vec<MigratedPosition>,
    pub unplaced: Vec<Binding>,
}

pub struct Migration {
    pub saved: SavedV2Parts,
    pub notes: Vec<String>,
    pub color_keys: Vec<(String, String)>,
}

fn device_key(b: &Binding) -> String {
    format!("{}:{}", b.kind.prefix(), b.name)
}

pub fn migrate_v1(master: Option<Binding>, devices: Vec<Binding>) -> Migration {
    let mut positions: Vec<MigratedPosition> = Vec::new();
    let (mut notes, mut keys, mut unplaced) = (Vec::new(), Vec::new(), Vec::new());
    let master_pos = master.as_ref().map(|_| PosId { group: PosGroup::Asio, index: 0 });
    if let (Some(m), Some(pos)) = (&master, master_pos) {
        keys.push((device_key(m), format!("pos:{pos}")));
    }
    for b in devices {
        let taken: Vec<PosId> = positions.iter().map(|(p, _, _)| *p).chain(master_pos).collect();
        let (pos, virt, binding) = match (b.kind, is_own_vasio_driver(&b.name)) {
            (DeviceKind::Asio, Some(i)) => {
                let pos = PosId { group: PosGroup::Vasio, index: i };
                let shape = (b.inputs, b.outputs);
                let vb = Binding { kind: DeviceKind::Vasio, name: vasio_device_name(pos, shape), ..b.clone() };
                notes.push(format!("{} (added as ASIO) is now {}", b.name, pos.label()));
                (Some(pos), Some((true, shape)), vb)
            }
            (DeviceKind::Vasio, _) => match parse_vasio(&b.name) {
                Ok((n, daw_in, daw_out)) if (1..=8).contains(&n) => {
                    let pos = PosId { group: PosGroup::Vasio, index: (n - 1) as u8 };
                    (Some(pos), Some((true, (daw_out as u32, daw_in as u32))), b.clone())
                }
                _ => (None, None, b.clone()),
            },
            (DeviceKind::Vaio, _) => (Some(PosId { group: PosGroup::Vaio, index: 0 }), Some((true, (2, 0))), b.clone()),
            (k, _) => (next_free(PosGroup::for_kind(k), &taken), None, b.clone()),
        };
        match pos.filter(|p| !taken.contains(p)) {
            Some(pos) => {
                keys.push((device_key(&b), format!("pos:{pos}")));
                positions.push((pos, virt, Some(binding)));
            }
            None => {
                notes.push(format!("{} could not be given a position (all in use): kept, not opened", b.name));
                unplaced.push(b);
            }
        }
    }
    Migration {
        saved: SavedV2Parts { master: master_pos, master_binding: master, positions, unplaced },
        notes,
        color_keys: keys,
    }
}

pub fn backup(path: &Path) -> std::io::Result<PathBuf> {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("backup");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let to = path.with_file_name(format!("{stem}.v1.{ext}"));
    if !to.exists() {
        std::fs::copy(path, &to)?;
    }
    Ok(to)
}
