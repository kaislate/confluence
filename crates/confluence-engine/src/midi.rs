//! MIDI devices: a provider (WinMM on Windows), and the hub that keeps every
//! input open, hands their messages to the control loop, and sends feedback
//! to outputs, opening each one when it is first needed.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often the hub looks for MIDI inputs that appeared or vanished.
pub const RESCAN: Duration = Duration::from_secs(2);

/// A short MIDI message received from an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiEvent {
    pub device: String,
    pub bytes: Vec<u8>,
}

/// An open MIDI input: messages go to the channel given when it was opened;
/// dropping it closes it.
pub trait MidiInput: Send {
    /// False once the device behind this handle is gone (unplugged, even if it
    /// is back under the same name: a new handle is needed).
    fn alive(&self) -> bool {
        true
    }
}

/// A device name without the " (2)", " (3)"… given to identical devices.
fn base(name: &str) -> &str {
    match name.rsplit_once(" (") {
        Some((b, n)) if n.strip_suffix(')').is_some_and(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit())) => {
            b
        }
        _ => name,
    }
}

/// An open MIDI output; dropping it closes it.
pub trait MidiOutput: Send {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String>;
}

/// Where MIDI devices come from.
pub trait MidiProvider: Send {
    fn inputs(&self) -> Vec<String>;
    fn outputs(&self) -> Vec<String>;
    fn open_input(&self, name: &str, events: mpsc::Sender<MidiEvent>) -> Result<Box<dyn MidiInput>, String>;
    fn open_output(&self, name: &str) -> Result<Box<dyn MidiOutput>, String>;
}

/// The output of the same device as input `name`: the same name, or (for
/// drivers that name the two halves apart) `MIDIIN` read as `MIDIOUT`.
pub fn output_for(input: &str, outputs: &[String]) -> Option<String> {
    if outputs.iter().any(|o| o == input) {
        return Some(input.to_string());
    }
    let alt = input.replace("MIDIIN", "MIDIOUT");
    outputs.iter().find(|o| **o == alt).cloned()
}

/// Every MIDI input open, and outputs opened for feedback.
pub struct MidiHub {
    provider: Box<dyn MidiProvider>,
    inputs: BTreeMap<String, Box<dyn MidiInput>>,
    outputs: BTreeMap<String, Box<dyn MidiOutput>>,
    tx: mpsc::Sender<MidiEvent>,
    rx: mpsc::Receiver<MidiEvent>,
    last_scan: Option<Instant>,
    /// The outputs seen at the last scan (not listed again for every message).
    output_names: Vec<String>,
}

impl MidiHub {
    pub fn new(provider: Box<dyn MidiProvider>) -> MidiHub {
        let (tx, rx) = mpsc::channel();
        MidiHub {
            provider,
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            tx,
            rx,
            last_scan: None,
            output_names: Vec::new(),
        }
    }

    /// Opens inputs that appeared and closes ones that vanished or went dead.
    /// Identical devices are told apart only by order, so when their number
    /// changes all of them are reopened under the names they have now.
    /// Returns the inputs opened (new or reopened).
    pub fn scan(&mut self) -> Vec<String> {
        let present = self.provider.inputs();
        self.output_names = self.provider.outputs();
        let count = |names: &mut dyn Iterator<Item = &String>, b: &str| names.filter(|n| base(n) == b).count();
        let shifted: Vec<String> = present
            .iter()
            .chain(self.inputs.keys())
            .map(|n| base(n).to_string())
            .filter(|b| count(&mut present.iter(), b) != count(&mut self.inputs.keys(), b))
            .collect();
        self.inputs
            .retain(|name, input| present.contains(name) && input.alive() && !shifted.iter().any(|b| b == base(name)));
        let outputs = self.output_names.clone();
        self.outputs.retain(|name, _| outputs.contains(name));
        let mut opened = Vec::new();
        for name in present {
            if !self.inputs.contains_key(&name) {
                match self.provider.open_input(&name, self.tx.clone()) {
                    Ok(input) => {
                        self.inputs.insert(name.clone(), input);
                        opened.push(name);
                    }
                    Err(e) => eprintln!("confluence-engine: warning: MIDI input {name} could not be opened: {e}"),
                }
            }
        }
        opened.sort();
        opened
    }

    /// Rescans at most every [`RESCAN`]; returns the inputs opened.
    pub fn tick(&mut self, now: Instant) -> Vec<String> {
        if self.last_scan.is_some_and(|t| now.saturating_duration_since(t) < RESCAN) {
            return Vec::new();
        }
        self.last_scan = Some(now);
        self.scan()
    }

    /// Messages received since the last call, in order.
    pub fn events(&mut self) -> Vec<MidiEvent> {
        self.rx.try_iter().collect()
    }

    /// The inputs open now.
    pub fn input_names(&self) -> Vec<String> {
        self.inputs.keys().cloned().collect()
    }

    /// Sends `bytes` to the output of the device that has input `device`,
    /// opening it if needed. False if it could not be sent (no such output, or
    /// it failed: then it is closed, and opened again next time).
    pub fn send(&mut self, device: &str, bytes: &[u8]) -> bool {
        let Some(name) = output_for(device, &self.output_names) else { return false };
        if !self.outputs.contains_key(&name) {
            match self.provider.open_output(&name) {
                Ok(out) => {
                    self.outputs.insert(name.clone(), out);
                }
                Err(_) => return false,
            }
        }
        let Some(out) = self.outputs.get_mut(&name) else { return false };
        if out.send(bytes).is_ok() {
            return true;
        }
        self.outputs.remove(&name);
        false
    }
}

#[cfg(windows)]
pub use winmm::WinmmProvider;

#[cfg(windows)]
mod winmm {
    use std::sync::mpsc;

    use windows::Win32::Media::Audio::{
        midiInClose, midiInGetDevCapsW, midiInGetID, midiInGetNumDevs, midiInOpen, midiInReset, midiInStart,
        midiInStop, midiOutClose, midiOutGetDevCapsW, midiOutGetNumDevs, midiOutOpen, midiOutReset, midiOutShortMsg,
        CALLBACK_FUNCTION, CALLBACK_NULL, HMIDIIN, HMIDIOUT, MIDIINCAPSW, MIDIOUTCAPSW,
    };
    use windows::Win32::Media::MM_MIM_DATA;

    use super::{MidiEvent, MidiInput, MidiOutput, MidiProvider};

    /// MIDI through WinMM (on Windows 11 with Windows MIDI Services, WinMM
    /// runs on the new stack).
    pub struct WinmmProvider;

    fn name(raw: &[u16]) -> String {
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        String::from_utf16_lossy(&raw[..end])
    }

    /// Device names, with " (2)", " (3)"… for repeats, in device order.
    fn unique(names: Vec<String>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for n in names {
            let mut candidate = n.clone();
            let mut k = 2;
            while out.contains(&candidate) {
                candidate = format!("{n} ({k})");
                k += 1;
            }
            out.push(candidate);
        }
        out
    }

    fn input_names() -> Vec<String> {
        // SAFETY: plain queries with properly sized structs.
        let n = unsafe { midiInGetNumDevs() };
        unique(
            (0..n)
                .filter_map(|i| {
                    let mut caps = MIDIINCAPSW::default();
                    let r =
                        unsafe { midiInGetDevCapsW(i as usize, &mut caps, std::mem::size_of::<MIDIINCAPSW>() as u32) };
                    (r == 0).then(|| {
                        let raw = caps.szPname; // copied out: the struct is packed
                        name(&raw)
                    })
                })
                .collect(),
        )
    }

    fn output_names() -> Vec<String> {
        // SAFETY: as above.
        let n = unsafe { midiOutGetNumDevs() };
        unique(
            (0..n)
                .filter_map(|i| {
                    let mut caps = MIDIOUTCAPSW::default();
                    let r = unsafe {
                        midiOutGetDevCapsW(i as usize, &mut caps, std::mem::size_of::<MIDIOUTCAPSW>() as u32)
                    };
                    (r == 0).then(|| {
                        let raw = caps.szPname; // copied out: the struct is packed
                        name(&raw)
                    })
                })
                .collect(),
        )
    }

    /// What the input callback needs: the device's name and where to send.
    struct Sink {
        device: String,
        events: mpsc::Sender<MidiEvent>,
    }

    /// WinMM calls this on its own thread: forward and return, never block.
    unsafe extern "system" fn on_input(_h: HMIDIIN, msg: u32, instance: usize, param1: usize, _param2: usize) {
        if msg != MM_MIM_DATA {
            return;
        }
        // SAFETY: `instance` is the `Sink` boxed for this input, alive until it is closed.
        let Some(sink) = (unsafe { (instance as *const Sink).as_ref() }) else { return };
        let packed = param1 as u32;
        let status = (packed & 0xFF) as u8;
        let len = match status & 0xF0 {
            0xC0 | 0xD0 => 2,
            0x80..=0xE0 => 3,
            _ => 1,
        };
        let bytes = packed.to_le_bytes()[..len].to_vec();
        let _ = sink.events.send(MidiEvent { device: sink.device.clone(), bytes });
    }

    struct Input {
        handle: HMIDIIN,
        sink: *mut Sink,
    }

    // SAFETY: the handle may be closed from any thread; the sink is only freed after closing.
    unsafe impl Send for Input {}

    impl MidiInput for Input {
        fn alive(&self) -> bool {
            let mut id = 0u32;
            // SAFETY: our handle; fails once its device is gone.
            unsafe { midiInGetID(self.handle, &mut id) == 0 }
        }
    }

    impl Drop for Input {
        fn drop(&mut self) {
            // SAFETY: our handle; after midiInClose no callback runs, so the sink can go.
            unsafe {
                midiInStop(self.handle);
                midiInReset(self.handle);
                midiInClose(self.handle);
                drop(Box::from_raw(self.sink));
            }
        }
    }

    struct Output(HMIDIOUT);

    // SAFETY: WinMM output handles may be used from any thread.
    unsafe impl Send for Output {}

    impl MidiOutput for Output {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            let mut packed = [0u8; 4];
            for (d, s) in packed.iter_mut().zip(bytes.iter().take(3)) {
                *d = *s;
            }
            // SAFETY: our open handle.
            match unsafe { midiOutShortMsg(self.0, u32::from_le_bytes(packed)) } {
                0 => Ok(()),
                e => Err(format!("WinMM error {e}")),
            }
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            // SAFETY: our handle.
            unsafe {
                midiOutReset(self.0);
                midiOutClose(self.0);
            }
        }
    }

    impl MidiProvider for WinmmProvider {
        fn inputs(&self) -> Vec<String> {
            input_names()
        }

        fn outputs(&self) -> Vec<String> {
            output_names()
        }

        fn open_input(&self, name: &str, events: mpsc::Sender<MidiEvent>) -> Result<Box<dyn MidiInput>, String> {
            let id = input_names().iter().position(|n| n == name).ok_or_else(|| format!("{name} is gone"))?;
            let sink = Box::into_raw(Box::new(Sink { device: name.to_string(), events }));
            let mut handle = HMIDIIN::default();
            // SAFETY: the callback and its instance stay valid until the input is closed.
            let r = unsafe {
                midiInOpen(
                    &mut handle,
                    id as u32,
                    Some(on_input as *const () as usize),
                    Some(sink as usize),
                    CALLBACK_FUNCTION,
                )
            };
            if r != 0 {
                // SAFETY: not handed to WinMM.
                drop(unsafe { Box::from_raw(sink) });
                return Err(format!("WinMM error {r}"));
            }
            // SAFETY: an open handle.
            unsafe { midiInStart(handle) };
            Ok(Box::new(Input { handle, sink }))
        }

        fn open_output(&self, name: &str) -> Result<Box<dyn MidiOutput>, String> {
            let id = output_names().iter().position(|n| n == name).ok_or_else(|| format!("{name} is gone"))?;
            let mut handle = HMIDIOUT::default();
            // SAFETY: no callback.
            let r = unsafe { midiOutOpen(&mut handle, id as u32, None, None, CALLBACK_NULL) };
            if r != 0 {
                return Err(format!("WinMM error {r}"));
            }
            Ok(Box::new(Output(handle)))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Lists devices (opens none): works with or without MIDI hardware.
        #[test]
        fn devices_can_be_listed() {
            let p = WinmmProvider;
            let _ = (p.inputs(), p.outputs());
        }

        #[test]
        fn repeated_names_are_told_apart() {
            let names = unique(vec!["Pad".into(), "Pad".into(), "Keys".into(), "Pad".into()]);
            assert_eq!(names, ["Pad", "Pad (2)", "Keys", "Pad (3)"]);
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A provider for tests: devices come and go as the test says; messages
    //! are pushed by the test; what is sent to outputs is recorded.
    use std::sync::{mpsc, Arc, Mutex};

    use super::{MidiEvent, MidiInput, MidiOutput, MidiProvider};

    #[derive(Default)]
    pub struct World {
        pub inputs: Vec<String>,
        pub outputs: Vec<String>,
        pub open: Vec<(String, mpsc::Sender<MidiEvent>)>,
        /// Every open_input call, in order.
        pub opens: Vec<String>,
        /// Inputs whose open handles have gone dead (unplugged meanwhile).
        pub dead: Vec<String>,
        pub opened_outputs: Vec<String>,
        pub sent: Vec<(String, Vec<u8>)>,
    }

    #[derive(Clone, Default)]
    pub struct Fake(pub Arc<Mutex<World>>);

    impl Fake {
        /// `device` sends `bytes`.
        pub fn play(&self, device: &str, bytes: &[u8]) {
            let w = self.0.lock().unwrap_or_else(|p| p.into_inner());
            for (name, tx) in &w.open {
                if name == device {
                    let _ = tx.send(MidiEvent { device: device.into(), bytes: bytes.to_vec() });
                }
            }
        }
    }

    struct In(Arc<Mutex<World>>, String, usize);
    impl MidiInput for In {
        fn alive(&self) -> bool {
            let w = self.0.lock().unwrap_or_else(|p| p.into_inner());
            // Dead once marked, until reopened (a newer open of the same name).
            !(w.dead.contains(&self.1) && w.opens.iter().filter(|n| **n == self.1).count() == self.2)
        }
    }
    impl Drop for In {
        fn drop(&mut self) {
            let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
            w.open.retain(|(n, _)| *n != self.1);
        }
    }

    struct Out(Arc<Mutex<World>>, String);
    impl MidiOutput for Out {
        fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
            let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
            w.sent.push((self.1.clone(), bytes.to_vec()));
            Ok(())
        }
    }

    impl MidiProvider for Fake {
        fn inputs(&self) -> Vec<String> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).inputs.clone()
        }
        fn outputs(&self) -> Vec<String> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).outputs.clone()
        }
        fn open_input(&self, name: &str, events: mpsc::Sender<MidiEvent>) -> Result<Box<dyn MidiInput>, String> {
            let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
            w.open.push((name.into(), events));
            w.opens.push(name.into());
            let nth = w.opens.iter().filter(|n| *n == name).count();
            Ok(Box::new(In(self.0.clone(), name.into(), nth)))
        }
        fn open_output(&self, name: &str) -> Result<Box<dyn MidiOutput>, String> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).opened_outputs.push(name.into());
            Ok(Box::new(Out(self.0.clone(), name.into())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;

    fn hub_with(inputs: &[&str], outputs: &[&str]) -> (MidiHub, Fake) {
        let fake = Fake::default();
        {
            let mut w = fake.0.lock().unwrap();
            w.inputs = inputs.iter().map(|s| s.to_string()).collect();
            w.outputs = outputs.iter().map(|s| s.to_string()).collect();
        }
        (MidiHub::new(Box::new(fake.clone())), fake)
    }

    fn opened(fake: &Fake, name: &str) -> usize {
        fake.0.lock().unwrap().opens.iter().filter(|n| *n == name).count()
    }

    #[test]
    fn every_input_is_opened_and_its_messages_arrive_in_order() {
        let (mut hub, fake) = hub_with(&["Pad", "Keys"], &[]);
        assert_eq!(hub.scan(), ["Keys", "Pad"]);
        assert_eq!(hub.input_names(), ["Keys", "Pad"]);
        fake.play("Pad", &[0xB0, 7, 1]);
        fake.play("Keys", &[0xB0, 7, 2]);
        fake.play("Pad", &[0xB0, 7, 3]);
        let got: Vec<u8> = hub.events().iter().map(|e| e.bytes[2]).collect();
        assert_eq!(got, [1, 2, 3]);
        assert!(hub.scan().is_empty(), "nothing changed");
    }

    #[test]
    fn an_unplugged_input_is_closed_and_reopened_when_it_returns() {
        let (mut hub, fake) = hub_with(&["Pad"], &[]);
        hub.scan();
        fake.0.lock().unwrap().inputs.clear();
        assert!(hub.scan().is_empty());
        assert!(hub.input_names().is_empty());
        assert!(fake.0.lock().unwrap().open.is_empty(), "closed");
        fake.0.lock().unwrap().inputs.push("Pad".into());
        assert_eq!(hub.scan(), ["Pad"]);
        fake.play("Pad", &[0xB0, 1, 64]);
        assert_eq!(hub.events().len(), 1, "works again");
    }

    #[test]
    fn rescans_happen_at_most_every_two_seconds() {
        let (mut hub, fake) = hub_with(&[], &[]);
        let t0 = Instant::now();
        assert!(hub.tick(t0).is_empty());
        fake.0.lock().unwrap().inputs.push("Pad".into());
        assert!(hub.tick(t0 + Duration::from_millis(500)).is_empty(), "too soon");
        assert_eq!(hub.tick(t0 + RESCAN), ["Pad"]);
    }

    /// Two identical controllers: when the first is unplugged, the second takes
    /// its name; it must be reopened, not left behind a dead handle.
    #[test]
    fn identical_devices_are_reopened_when_their_names_shift() {
        let (mut hub, fake) = hub_with(&["Pad", "Pad (2)"], &[]);
        hub.scan();
        fake.0.lock().unwrap().inputs = vec!["Pad".into()];
        assert_eq!(hub.scan(), ["Pad"], "reopened under the name it has now");
        assert_eq!(opened(&fake, "Pad"), 2);
        assert_eq!(hub.input_names(), ["Pad"]);
    }

    /// A device that went away and came back between two rescans: its old
    /// handle is dead, so it is reopened.
    #[test]
    fn a_dead_input_is_reopened() {
        let (mut hub, fake) = hub_with(&["Pad"], &[]);
        hub.scan();
        fake.0.lock().unwrap().dead.push("Pad".into());
        assert_eq!(hub.scan(), ["Pad"]);
        assert_eq!(opened(&fake, "Pad"), 2);
    }

    #[test]
    fn feedback_goes_to_the_same_devices_output_opened_once() {
        let (mut hub, fake) = hub_with(&["Pad", "MIDIIN2 (Keys)"], &["Pad", "MIDIOUT2 (Keys)"]);
        hub.scan();
        assert!(hub.send("Pad", &[0xB0, 7, 100]));
        assert!(hub.send("Pad", &[0xB0, 7, 101]));
        assert!(hub.send("MIDIIN2 (Keys)", &[0xB1, 1, 5]));
        assert!(!hub.send("Nowhere", &[0xB0, 1, 1]), "no output: not sent");
        let w = fake.0.lock().unwrap();
        assert_eq!(w.opened_outputs, ["Pad", "MIDIOUT2 (Keys)"]);
        assert_eq!(w.sent.len(), 3);
        assert_eq!(w.sent[2], ("MIDIOUT2 (Keys)".to_string(), vec![0xB1, 1, 5]));
    }
}
