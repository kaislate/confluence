//! Plugin editor windows, with the test plugin's own editor (real windows).
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use confluence_core::buffer::PlanarBuffer;
use confluence_core::processor::{BusIo, Processor};
use confluence_plugin_host::{PluginLink, PluginThread, Source};
use confluence_test_plugin::{DEACTIVATIONS, GAIN_ID, PARAM_GAIN, PLAIN_ID};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, PostMessageW, SendMessageW, WM_CLOSE, WM_LBUTTONDOWN,
};

fn source() -> Source {
    Source::InProcess(|| {
        clack_host::entry::PluginEntry::load_from_clack::<confluence_test_plugin::Entry>(c"test_plugin.dll")
            .map_err(|e| e.to_string())
    })
}

fn load(t: &PluginThread, id: &str) -> (PluginLink, Box<dyn Processor>) {
    t.load(source(), id, 48_000.0, 64, 2).unwrap()
}

fn window(title: &str) -> Option<HWND> {
    // SAFETY: a read-only lookup.
    unsafe { FindWindowW(None, &HSTRING::from(title)) }.ok()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_editor_opens_in_a_titled_window_and_closes() {
    let t = PluginThread::start().unwrap();
    let (mut link, _p) = load(&t, GAIN_ID);
    assert!(link.has_editor());
    assert!(!link.editor_open());
    let title = "Confluence Test Gain — open and close";
    link.show_editor(title).unwrap();
    let w = window(title).expect("a window with the title");
    // SAFETY: a read-only lookup of the plugin's child.
    let child = unsafe { FindWindowExW(Some(w), None, w!("ConfluenceTestGainEditor"), None) };
    assert!(child.is_ok(), "the plugin's editor is inside it");
    assert!(link.editor_open());
    link.hide_editor();
    assert!(window(title).is_none());
    assert!(!link.editor_open());
}

#[test]
fn closing_the_window_closes_the_editor() {
    let t = PluginThread::start().unwrap();
    let (mut link, _p) = load(&t, GAIN_ID);
    let title = "Confluence Test Gain — closed by the user";
    link.show_editor(title).unwrap();
    let w = window(title).unwrap();
    // SAFETY: asking another thread's window to close.
    unsafe { PostMessageW(Some(w), WM_CLOSE, WPARAM(0), LPARAM(0)) }.unwrap();
    wait_for("the editor to close", || !link.editor_open() && window(title).is_none());
}

#[test]
fn showing_twice_keeps_one_window() {
    let t = PluginThread::start().unwrap();
    let (mut link, _p) = load(&t, GAIN_ID);
    let title = "Confluence Test Gain — shown twice";
    link.show_editor(title).unwrap();
    let first = window(title).unwrap();
    link.show_editor(title).unwrap();
    assert_eq!(window(title), Some(first), "the same window, brought to the front");
    link.hide_editor();
}

#[test]
fn unloading_with_the_editor_open_removes_the_window() {
    let t = PluginThread::start().unwrap();
    let (mut link, mut p) = load(&t, GAIN_ID);
    let title = "Confluence Test Gain — unloaded";
    link.show_editor(title).unwrap();
    assert!(window(title).is_some());
    let before = DEACTIVATIONS.load(Ordering::Relaxed);
    p.stop();
    t.reclaim(p);
    drop(link);
    wait_for("the window to go", || window(title).is_none());
    wait_for("the plugin to be deactivated", || DEACTIVATIONS.load(Ordering::Relaxed) > before);
}

#[test]
fn a_change_made_in_the_editor_is_reported_as_edited() {
    let t = PluginThread::start().unwrap();
    let (mut link, mut p) = load(&t, GAIN_ID);
    let title = "Confluence Test Gain — clicked";
    link.show_editor(title).unwrap();
    let w = window(title).unwrap();
    // SAFETY: lookup, then a click delivered to the plugin's own window.
    let child = unsafe { FindWindowExW(Some(w), None, w!("ConfluenceTestGainEditor"), None) }.unwrap();
    unsafe { SendMessageW(child, WM_LBUTTONDOWN, Some(WPARAM(0)), Some(LPARAM(0))) };
    let sends = PlanarBuffer::new(2, 64);
    let mut returns = PlanarBuffer::new(2, 64);
    p.process(BusIo::new(&sends, 0, &mut returns, 0, 2)).unwrap();
    wait_for("the change", || {
        link.poll();
        link.params()[0].value == confluence_test_plugin::gui::EDITOR_CLICK_GAIN
    });
    assert_eq!(link.take_edited(), vec![(PARAM_GAIN, -12.0)]);
    assert!(link.take_edited().is_empty(), "taken once");
    link.set_param(PARAM_GAIN, -3.0).unwrap();
    p.process(BusIo::new(&sends, 0, &mut returns, 0, 2)).unwrap();
    link.poll();
    assert!(link.take_edited().is_empty(), "changes the host made are not edits from the editor");
    link.hide_editor();
}

#[test]
fn a_plugin_without_an_editor_says_so() {
    let t = PluginThread::start().unwrap();
    let (mut link, _p) = load(&t, PLAIN_ID);
    assert!(!link.has_editor());
    let err = link.show_editor("Confluence Test Plain — none").unwrap_err();
    assert!(err.contains("has no editor"), "{err}");
}
