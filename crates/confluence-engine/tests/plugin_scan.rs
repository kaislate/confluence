//! Plugin discovery and the load check, run as subprocesses of the real engine
//! binary on the real test-plugin file.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use confluence_engine::plugins::{self, Scanner};

fn engine() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_confluence-engine"))
}

/// `target/debug/confluence_test_plugin.dll`, built by `cargo build --workspace`.
fn test_plugin() -> PathBuf {
    let path = engine().parent().unwrap().join("confluence_test_plugin.dll");
    assert!(path.is_file(), "{} is missing: run `cargo build --workspace` first", path.display());
    path
}

#[test]
fn scanning_the_file_lists_both_test_plugins() {
    let found = plugins::describe(&engine(), &test_plugin()).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Confluence Test Gain",
            "Confluence Test Crash",
            "Confluence Test Exit",
            "Confluence Test Plain",
            "Confluence Test Echo"
        ]
    );
    assert!(found.iter().all(|p| p.vendor == "Confluence" && p.path.ends_with("confluence_test_plugin.dll")));
}

#[test]
fn the_load_check_passes_a_good_plugin_and_catches_a_crashing_one() {
    let file = test_plugin();
    plugins::check(&engine(), &file, "dev.confluence.test.gain", 48_000.0, 256).unwrap();
    let err = plugins::check(&engine(), &file, "dev.confluence.test.crash", 48_000.0, 256).unwrap_err();
    assert!(err.contains("crashed while loading; it was not loaded"), "{err}");
    // Ending the process "successfully" during the check is not a pass.
    let err = plugins::check(&engine(), &file, "dev.confluence.test.exit", 48_000.0, 256).unwrap_err();
    assert!(err.contains("it was not loaded"), "{err}");
    let err = plugins::check(&engine(), &file, "no.such.plugin", 48_000.0, 256).unwrap_err();
    assert!(err.contains("has no plugin no.such.plugin"), "{err}");
}

#[test]
fn the_scanner_finds_clap_files_and_lists_bad_ones() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("Vendor");
    std::fs::create_dir(&sub).unwrap();
    std::fs::copy(test_plugin(), sub.join("Test.clap")).unwrap();
    std::fs::write(dir.path().join("bad.clap"), b"not a plugin").unwrap();
    std::fs::write(dir.path().join("readme.txt"), b"ignored").unwrap();
    let scanner = Scanner::start(engine(), vec![dir.path().to_path_buf()]);
    let deadline = Instant::now() + Duration::from_secs(30);
    let (found, bad) = loop {
        let (found, bad) = scanner.list();
        if found.len() == 5 && bad.len() == 1 {
            break (found, bad);
        }
        assert!(Instant::now() < deadline, "scan did not finish: {found:?} {bad:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(found[0].name, "Confluence Test Crash", "sorted by name");
    assert!(bad[0].0.ends_with("bad.clap"), "{bad:?}");
}

#[test]
fn default_folders_follow_clap_path() {
    std::env::set_var("CLAP_PATH", r"C:\one;C:\two");
    let dirs = plugins::default_dirs();
    assert!(dirs.contains(&PathBuf::from(r"C:\one")) && dirs.contains(&PathBuf::from(r"C:\two")), "{dirs:?}");
    assert!(dirs.iter().any(|d| d.ends_with(r"Common Files\CLAP") || d.ends_with("CLAP")), "{dirs:?}");
}
