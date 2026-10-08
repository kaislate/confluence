#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use confluence_api::PosId;
use confluence_engine::devices::Binding;
use confluence_engine::migrate::{backup, migrate_v1, Migration};

fn p(s: &str) -> PosId {
    s.parse().unwrap()
}
fn load(name: &str) -> (Option<Binding>, Vec<Binding>) {
    #[derive(serde::Deserialize)]
    struct V1 {
        master: Option<Binding>,
        devices: Vec<Binding>,
    }
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let v: V1 = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    (v.master, v.devices)
}
type Placed = (Option<(bool, (u32, u32))>, Binding);

fn placed(m: &Migration, pos: &str) -> Placed {
    let (_, v, b) =
        m.saved.positions.iter().find(|(q, _, _)| *q == p(pos)).unwrap_or_else(|| panic!("{pos} not placed"));
    (*v, b.clone().unwrap())
}

#[test]
fn the_users_setup_lands_in_positions_with_its_channels_unchanged() {
    let (master, devices) = load("devices-v1-user.json");
    let m = migrate_v1(master, devices);
    assert_eq!(m.saved.master, Some(p("asio:1")), "the GoXLR master takes ASIO 1");
    assert_eq!(m.saved.master_binding.as_ref().unwrap().first_input, 25);
    for (pos, first_out) in [("win-out:1", 0), ("win-out:2", 2), ("win-out:3", 4)] {
        let (_, b) = placed(&m, pos);
        assert_eq!((b.first_output, b.outputs), (first_out, 2), "{pos}");
    }
    let (_, b) = placed(&m, "win-in:1");
    assert_eq!((b.first_input, b.inputs), (0, 2));
    assert_eq!(placed(&m, "net-in:1").1.name, "Lilith/Back");
    // The ASIO-added VASIO pair becomes VASIO A, on; its channels are unchanged.
    let (v, b) = placed(&m, "vasio:A");
    assert_eq!(v, Some((true, (2, 2))));
    assert_eq!(b.kind, confluence_api::DeviceKind::Vasio);
    assert_eq!((b.first_input, b.inputs, b.first_output, b.outputs), (23, 2, 16, 2));
    assert!(m.notes.iter().any(|n| n.contains("Confluence VASIO 1") && n.contains("VASIO A")), "{:?}", m.notes);
    assert!(m.saved.unplaced.is_empty());
    assert!(m.color_keys.contains(&("asio:Confluence VASIO 1".to_string(), "pos:vasio:A".to_string())));
    assert!(m.color_keys.contains(&("wasapi-out:Game (4- TC-HELICON GoXLR)".to_string(), "pos:win-out:1".to_string())));
}

#[test]
fn vasio_vaio_and_overflow_are_kept() {
    let (master, devices) = load("devices-v1-overflow.json");
    let m = migrate_v1(master, devices);
    for i in 1..=8 {
        placed(&m, &format!("win-out:{i}"));
    }
    assert_eq!(m.saved.unplaced.len(), 1, "the ninth output is kept, unplaced");
    assert!(m.notes.iter().any(|n| n.contains("Out 9")));
    let (v, b) = placed(&m, "vasio:C");
    assert_eq!(v, Some((true, (2, 8))), "engine-side shape (DAW outputs, DAW inputs)");
    assert_eq!((b.first_input, b.first_output), (40, 40));
    assert_eq!(placed(&m, "vaio:A").0.map(|x| x.0), Some(true));
}

#[test]
fn a_backup_is_written_once_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("devices.json");
    std::fs::write(&f, "old").unwrap();
    let b = backup(&f).unwrap();
    assert_eq!(b.file_name().unwrap(), "devices.v1.json");
    std::fs::write(&f, "newer").unwrap();
    backup(&f).unwrap();
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "old", "the first backup is kept");
}
