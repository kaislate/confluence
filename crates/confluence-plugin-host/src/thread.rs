//! The plugin thread: CLAP wants every main-thread call on one thread, so one
//! thread owns every plugin instance and runs requests sent to it.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use clack_extensions::audio_ports::{AudioPortInfoBuffer, PluginAudioPorts};
use clack_extensions::latency::PluginLatency;
use clack_extensions::params::{ParamInfoBuffer, ParamInfoFlags, PluginParams};
use clack_extensions::state::PluginState;
use clack_host::prelude::*;
use confluence_api::{ParamState, PluginInfo};
use confluence_core::mailbox;
use confluence_core::processor::Processor;

use crate::editor::{self, Editor, WinEvent};
use crate::host::{host_info, GuiRequests, Host, Main, Shared};
use crate::processor::ClapProcessor;
use crate::wake::Wake;

/// How long a caller waits for the plugin thread.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Saving a state happens under the engine's lock: keep it short, and fall
/// back to the last state saved when the plugin thread is busy.
const SAVE_TIMEOUT: Duration = Duration::from_secs(1);
/// Loading can read files and allocate a lot.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the plugin thread looks for plugins asking to be called back.
const POLL: Duration = Duration::from_millis(20);
/// Parameter changes in flight to one plugin's audio side.
const PARAM_RING: usize = 1024;
/// Values one plugin reports in flight to the engine.
const REPORT_RING: usize = 1024;

/// Where a plugin file comes from.
#[derive(Clone)]
pub enum Source {
    /// A `.clap` file.
    File(PathBuf),
    /// A plugin compiled into this program (tests).
    #[doc(hidden)]
    InProcess(fn() -> Result<PluginEntry, String>),
}

impl Source {
    fn entry(&self) -> Result<PluginEntry, String> {
        match self {
            Source::File(p) => {
                // SAFETY: loading a CLAP file runs its code; the engine only loads
                // files that passed the load check in a separate process.
                unsafe { PluginEntry::load(p) }.map_err(|e| format!("{} could not be loaded: {e}", p.display()))
            }
            Source::InProcess(f) => f(),
        }
    }

    fn path(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::InProcess(_) => "(built in)".into(),
        }
    }
}

/// A plugin loaded on the plugin thread, as the load request returns it.
struct Loaded {
    plugin: u64,
    info: PluginInfo,
    latency: u32,
    has_editor: bool,
    editor_open: Arc<AtomicBool>,
    params: Vec<ParamState>,
    processor: ClapProcessor,
    params_tx: mailbox::Sender<(u32, f64)>,
    reported_rx: mailbox::Receiver<(u32, f64)>,
}

enum Msg {
    Load { src: Source, id: String, rate: f64, block: u32, reply: mpsc::Sender<Result<Loaded, String>> },
    Text { plugin: u64, id: u32, value: f64, reply: mpsc::Sender<String> },
    Values { plugin: u64, reply: mpsc::Sender<Vec<(u32, f64)>> },
    SaveState { plugin: u64, reply: mpsc::Sender<Result<Vec<u8>, String>> },
    LoadState { plugin: u64, state: Vec<u8>, reply: mpsc::Sender<Result<(), String>> },
    Reclaim { plugin: u64, processor: Box<ClapProcessor> },
    ShowEditor { plugin: u64, title: String, reply: mpsc::Sender<Result<(), String>> },
    HideEditor { plugin: u64, reply: mpsc::Sender<()> },
    Forget { plugin: u64 },
}

/// A handle to the plugin thread. Cloning it is cheap; the thread ends when
/// every handle (and every link) is gone.
#[derive(Clone)]
pub struct PluginThread {
    tx: mpsc::Sender<Msg>,
    wake: Arc<Wake>,
}

impl PluginThread {
    /// Starts the plugin thread.
    pub fn start() -> std::io::Result<PluginThread> {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(Wake::new()?);
        let w = wake.clone();
        std::thread::Builder::new().name("confluence-plugins".into()).spawn(move || run(rx, w))?;
        Ok(PluginThread { tx, wake })
    }

    /// Queues a request and wakes the thread.
    fn send(&self, msg: Msg) -> Result<(), mpsc::SendError<Msg>> {
        let r = self.tx.send(msg);
        self.wake.set();
        r
    }

    /// Loads plugin `id` from `src`, activated at `rate` with blocks of up to
    /// `block` frames, for a bus of `channels` channels. Returns the engine's
    /// link to it and the processor for the bus.
    pub fn load(
        &self,
        src: Source,
        id: &str,
        rate: f64,
        block: u32,
        channels: u32,
    ) -> Result<(PluginLink, Box<dyn Processor>), String> {
        let _ = channels; // the processor maps whatever the bus has to the plugin's ports
        let loaded = self.call(LOAD_TIMEOUT, |reply| Msg::Load { src, id: id.to_string(), rate, block, reply })??;
        let link = PluginLink {
            thread: self.clone(),
            plugin: loaded.plugin,
            info: loaded.info,
            latency: loaded.latency,
            params: loaded.params,
            params_tx: loaded.params_tx,
            reported_rx: loaded.reported_rx,
            texts: Vec::new(),
            last_state: None,
            has_editor: loaded.has_editor,
            editor_open: loaded.editor_open,
            edited: Vec::new(),
        };
        Ok((link, Box::new(loaded.processor)))
    }

    /// Takes back a processor the audio side has finished with, to deactivate
    /// its plugin on this thread. Processors that are not plugins are dropped.
    pub fn reclaim(&self, processor: Box<dyn Processor>) {
        let any: Box<dyn std::any::Any + Send> = processor;
        if let Ok(p) = any.downcast::<ClapProcessor>() {
            let plugin = p.plugin;
            if let Err(mpsc::SendError(Msg::Reclaim { processor, .. })) =
                self.send(Msg::Reclaim { plugin, processor: p })
            {
                // The plugin thread is gone; nothing can deactivate it now.
                std::mem::forget(processor);
            }
        }
    }

    fn call<T>(&self, timeout: Duration, make: impl FnOnce(mpsc::Sender<T>) -> Msg) -> Result<T, String> {
        let (reply, rx) = mpsc::channel();
        self.send(make(reply)).map_err(|_| "the plugin thread has stopped".to_string())?;
        rx.recv_timeout(timeout).map_err(|_| "the plugin did not respond".to_string())
    }
}

/// The engine's side of one loaded plugin: its description, a cache of its
/// parameters, and the rings to and from its processor.
pub struct PluginLink {
    thread: PluginThread,
    plugin: u64,
    info: PluginInfo,
    latency: u32,
    params: Vec<ParamState>,
    params_tx: mailbox::Sender<(u32, f64)>,
    reported_rx: mailbox::Receiver<(u32, f64)>,
    /// Texts asked of the plugin thread and not yet answered: (param, reply).
    texts: Vec<(u32, mpsc::Receiver<String>)>,
    /// The last state chunk saved, for when the plugin thread is busy.
    last_state: Option<Vec<u8>>,
    has_editor: bool,
    /// Kept up to date by the plugin thread.
    editor_open: Arc<AtomicBool>,
    /// Writable values the plugin changed itself (in its editor), not yet taken.
    edited: Vec<(u32, f64)>,
}

impl PluginLink {
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    pub fn latency(&self) -> u32 {
        self.latency
    }

    /// The plugin's parameters (hidden ones left out), with their latest values.
    pub fn params(&self) -> &[ParamState] {
        &self.params
    }

    /// Sends a new value (clamped to the parameter's range) to the plugin.
    pub fn set_param(&mut self, id: u32, value: f64) -> Result<(), String> {
        let name = self.info.name.clone();
        let Some(p) = self.params.iter_mut().find(|p| p.id == id) else {
            return Err(format!("{name} has no parameter {id}"));
        };
        if p.read_only {
            return Err(format!("{name}'s {} cannot be set", p.name));
        }
        let mut v = value.clamp(p.min, p.max);
        if p.stepped {
            v = v.round();
        }
        self.params_tx.try_send((id, v)).map_err(|_| format!("{name} is not taking changes this fast"))?;
        p.value = v;
        self.refresh_text(id);
        Ok(())
    }

    /// Takes in values the plugin reported itself, and texts the plugin thread
    /// has answered. True if anything shown changed.
    pub fn poll(&mut self) -> bool {
        let mut shown = false;
        let mut answered = Vec::new();
        self.texts.retain(|(id, rx)| match rx.try_recv() {
            Ok(text) => {
                answered.push((*id, text));
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
        for (id, text) in answered {
            if let Some(p) = self.params.iter_mut().find(|p| p.id == id) {
                if p.text != text {
                    p.text = text;
                    shown = true;
                }
            }
        }
        let mut changed = Vec::new();
        while let Some((id, value)) = self.reported_rx.try_recv() {
            if let Some(p) = self.params.iter_mut().find(|p| p.id == id) {
                if p.value != value {
                    p.value = value;
                    if !changed.contains(&id) {
                        changed.push(id);
                    }
                    if !p.read_only {
                        self.edited.retain(|(q, _)| *q != id);
                        self.edited.push((id, value));
                    }
                }
            }
        }
        for id in &changed {
            self.refresh_text(*id);
        }
        shown || !changed.is_empty()
    }

    /// Whether the plugin has an editor of its own.
    pub fn has_editor(&self) -> bool {
        self.has_editor
    }

    /// Whether its editor is open now.
    pub fn editor_open(&self) -> bool {
        self.editor_open.load(Ordering::Acquire)
    }

    /// Opens the plugin's editor in a window titled `title`, or brings it to
    /// the front if it is open.
    pub fn show_editor(&mut self, title: &str) -> Result<(), String> {
        if !self.has_editor {
            return Err(format!("{} has no editor", self.info.name));
        }
        let (plugin, title) = (self.plugin, title.to_string());
        self.thread.call(REPLY_TIMEOUT, |reply| Msg::ShowEditor { plugin, title, reply })?
    }

    /// Closes the plugin's editor, if open.
    pub fn hide_editor(&mut self) {
        let plugin = self.plugin;
        let _ = self.thread.call(REPLY_TIMEOUT, |reply| Msg::HideEditor { plugin, reply });
    }

    /// Values the plugin changed itself (in its editor) since the last call,
    /// writable parameters only, newest value per parameter.
    pub fn take_edited(&mut self) -> Vec<(u32, f64)> {
        std::mem::take(&mut self.edited)
    }

    /// True while texts asked of the plugin are still on their way.
    pub fn texts_pending(&self) -> bool {
        !self.texts.is_empty()
    }

    /// The plugin's state chunk. If the plugin thread does not answer soon
    /// (it may be loading another plugin), the last state saved is returned.
    pub fn save_state(&mut self) -> Result<Vec<u8>, String> {
        let plugin = self.plugin;
        match self.thread.call(SAVE_TIMEOUT, |reply| Msg::SaveState { plugin, reply }) {
            Ok(Ok(state)) => {
                self.last_state = Some(state.clone());
                Ok(state)
            }
            Ok(Err(e)) => Err(e),
            Err(e) => self.last_state.clone().ok_or(e),
        }
    }

    /// Loads a state chunk, then re-reads every value.
    pub fn load_state(&mut self, state: &[u8]) -> Result<(), String> {
        let plugin = self.plugin;
        let state = state.to_vec();
        self.thread.call(REPLY_TIMEOUT, |reply| Msg::LoadState { plugin, state, reply })??;
        let values = self.thread.call(REPLY_TIMEOUT, |reply| Msg::Values { plugin, reply })?;
        for (id, value) in values {
            if let Some(p) = self.params.iter_mut().find(|p| p.id == id) {
                p.value = value;
            }
            self.refresh_text(id);
        }
        Ok(())
    }

    /// Asks the plugin how it shows a parameter's value; until it answers
    /// (collected by `poll`), the number is shown.
    fn refresh_text(&mut self, id: u32) {
        let Some(p) = self.params.iter_mut().find(|p| p.id == id) else { return };
        let value = p.value;
        p.text = format!("{value:.2}");
        let (reply, rx) = mpsc::channel();
        if self.thread.send(Msg::Text { plugin: self.plugin, id, value, reply }).is_ok() {
            self.texts.retain(|(q, _)| *q != id);
            self.texts.push((id, rx));
        }
    }
}

impl Drop for PluginLink {
    fn drop(&mut self) {
        let _ = self.thread.send(Msg::Forget { plugin: self.plugin });
    }
}

/// One instance and what still refers to it.
struct Slot {
    instance: PluginInstance<Host>,
    callback: Arc<AtomicBool>,
    name: String,
    gui: Arc<GuiRequests>,
    editor: Option<Editor>,
    editor_open: Arc<AtomicBool>,
    /// A link (engine side) still refers to it.
    linked: bool,
    /// Its processor is out on the audio side.
    processing: bool,
}

fn run(rx: mpsc::Receiver<Msg>, wake: Arc<Wake>) {
    #[cfg(windows)]
    windows_setup();
    let mut plugins: HashMap<u64, Slot> = HashMap::new();
    let mut next = 1u64;
    loop {
        wait(&rx, &wake);
        #[cfg(windows)]
        pump_messages();
        loop {
            match rx.try_recv() {
                Ok(msg) => handle(msg, &mut plugins, &mut next),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The engine is exiting. Editors close first. A processor
                    // may still be running on the audio thread: destroying its
                    // instance now could crash it, so those instances are left
                    // for the process exit.
                    for (_, mut slot) in plugins.drain() {
                        close_editor(&mut slot);
                        if slot.processing {
                            std::mem::forget(slot.instance);
                        }
                    }
                    return;
                }
            }
        }
        for ev in editor::take_events() {
            match ev {
                WinEvent::Close(id) => {
                    if let Some(slot) = plugins.get_mut(&id) {
                        close_editor(slot);
                    }
                }
                WinEvent::Resize(id, w, h) => {
                    if let Some(slot) = plugins.get_mut(&id) {
                        if let Some(ed) = &slot.editor {
                            editor::user_resized(&mut slot.instance, ed, w, h);
                        }
                    }
                }
            }
        }
        for slot in plugins.values_mut() {
            gui_requests(slot);
        }
        for slot in plugins.values_mut() {
            if slot.callback.swap(false, Ordering::AcqRel) {
                slot.instance.call_on_main_thread_callback();
            }
        }
    }
}

/// Waits for a request, a window message, or the callback poll interval.
#[cfg(windows)]
fn wait(_rx: &mpsc::Receiver<Msg>, wake: &Wake) {
    use windows::Win32::UI::WindowsAndMessaging::{MsgWaitForMultipleObjects, QS_ALLINPUT};
    // SAFETY: one valid event handle; a timeout, not an infinite wait.
    let _ = unsafe { MsgWaitForMultipleObjects(Some(&[wake.handle()]), false, POLL.as_millis() as u32, QS_ALLINPUT) };
}

#[cfg(not(windows))]
fn wait(_rx: &mpsc::Receiver<Msg>, _wake: &Wake) {
    std::thread::sleep(Duration::from_millis(1));
}

/// Runs every window message waiting for this thread (plugin editors).
#[cfg(windows)]
fn pump_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};
    let mut msg = MSG::default();
    // SAFETY: standard message loop on this thread's own queue.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Editor windows follow each monitor's scale.
#[cfg(windows)]
fn windows_setup() {
    use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    // SAFETY: affects only this thread.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Closes a slot's editor, if open: the plugin's GUI, then our window.
fn close_editor(slot: &mut Slot) {
    if let Some(ed) = slot.editor.take() {
        editor::close(&mut slot.instance, ed);
    }
    slot.editor_open.store(false, Ordering::Release);
}

/// Acts on what a plugin's editor asked for.
fn gui_requests(slot: &mut Slot) {
    let resize = slot.gui.resize.lock().ok().and_then(|mut r| r.take());
    let closed = slot.gui.closed.lock().ok().and_then(|mut c| c.take());
    let show = slot.gui.show.swap(false, Ordering::AcqRel);
    let hide = slot.gui.hide.swap(false, Ordering::AcqRel);
    if let Some(ed) = &slot.editor {
        if let Some((w, h)) = resize {
            editor::plugin_resized(ed, w, h);
        }
        if show || hide {
            editor::plugin_visibility(ed, show);
        }
    }
    if closed.is_some() {
        // A floating editor closed by the user: release it (destroy is safe
        // even if the plugin already did).
        close_editor(slot);
    }
}

/// Removes a plugin's slot: its editor closes first.
fn remove(plugins: &mut HashMap<u64, Slot>, plugin: u64) {
    if let Some(mut slot) = plugins.remove(&plugin) {
        close_editor(&mut slot);
    }
}

fn handle(msg: Msg, plugins: &mut HashMap<u64, Slot>, next: &mut u64) {
    match msg {
        Msg::Load { src, id, rate, block, reply } => {
            let plugin = *next;
            *next += 1;
            let _ = reply.send(load(plugin, &src, &id, rate, block, plugins));
        }
        Msg::Text { plugin, id, value, reply } => {
            let text = plugins.get_mut(&plugin).and_then(|s| text(&mut s.instance, id, value));
            let _ = reply.send(text.unwrap_or_else(|| format!("{value:.3}")));
        }
        Msg::Values { plugin, reply } => {
            let values = plugins.get_mut(&plugin).map(|s| values(&mut s.instance)).unwrap_or_default();
            let _ = reply.send(values);
        }
        Msg::SaveState { plugin, reply } => {
            let r = match plugins.get_mut(&plugin) {
                Some(s) => save_state(&mut s.instance),
                None => Err("the plugin is gone".into()),
            };
            let _ = reply.send(r);
        }
        Msg::LoadState { plugin, state, reply } => {
            let r = match plugins.get_mut(&plugin) {
                Some(s) => load_state(&mut s.instance, &state),
                None => Err("the plugin is gone".into()),
            };
            let _ = reply.send(r);
        }
        Msg::Reclaim { plugin, mut processor } => {
            if let Some(slot) = plugins.get_mut(&plugin) {
                if let Some(audio) = processor.audio.take() {
                    let stopped = audio.into_stopped();
                    slot.instance.deactivate(stopped);
                }
                slot.processing = false;
                if !slot.linked {
                    remove(plugins, plugin);
                }
            }
        }
        Msg::ShowEditor { plugin, title, reply } => {
            let r = match plugins.get_mut(&plugin) {
                Some(slot) => match &slot.editor {
                    Some(ed) => {
                        editor::front(&mut slot.instance, ed);
                        Ok(())
                    }
                    None => editor::open(&mut slot.instance, plugin, &title, &slot.name).map(|ed| {
                        slot.editor = Some(ed);
                        slot.editor_open.store(true, Ordering::Release);
                    }),
                },
                None => Err("the plugin is gone".into()),
            };
            let _ = reply.send(r);
        }
        Msg::HideEditor { plugin, reply } => {
            if let Some(slot) = plugins.get_mut(&plugin) {
                close_editor(slot);
            }
            let _ = reply.send(());
        }
        Msg::Forget { plugin } => {
            if let Some(slot) = plugins.get_mut(&plugin) {
                slot.linked = false;
                // Nothing can ask for the editor any more.
                close_editor(slot);
                if !slot.processing {
                    remove(plugins, plugin);
                }
            }
        }
    }
}

fn load(
    plugin: u64,
    src: &Source,
    id: &str,
    rate: f64,
    block: u32,
    plugins: &mut HashMap<u64, Slot>,
) -> Result<Loaded, String> {
    let entry = src.entry()?;
    let factory = entry.get_plugin_factory().ok_or_else(|| format!("{} has no plugins", src.path()))?;
    let descriptor = factory
        .plugin_descriptors()
        .find(|d| d.id().is_some_and(|i| i.to_bytes() == id.as_bytes()))
        .ok_or_else(|| format!("{} has no plugin {id}", src.path()))?;
    let text = |s: Option<&std::ffi::CStr>| s.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let info = PluginInfo {
        path: src.path(),
        id: id.to_string(),
        name: text(descriptor.name()),
        vendor: text(descriptor.vendor()),
        version: text(descriptor.version()),
    };
    let cid = CString::new(id).map_err(|e| e.to_string())?;
    let callback = Arc::new(AtomicBool::new(false));
    let gui = Arc::new(GuiRequests::default());
    let (flag, requests) = (callback.clone(), gui.clone());
    let mut instance = PluginInstance::<Host>::new(
        move |_| Shared { callback: flag, gui: requests },
        |_| Main,
        &entry,
        &cid,
        &host_info()?,
    )
    .map_err(|e| format!("{} could not start: {e}", info.name))?;

    let (ins, outs) = ports(&mut instance);
    if ins.is_empty() {
        return Err("this plugin has no audio input".into());
    }
    if outs.is_empty() {
        return Err("this plugin has no audio output".into());
    }
    let params = list_params(&mut instance);
    let config = PluginAudioConfiguration { sample_rate: rate, min_frames_count: 1, max_frames_count: block };
    let stopped = instance.activate(|_, _| (), config).map_err(|e| format!("{} could not start: {e}", info.name))?;
    let latency = {
        let handle = instance.plugin_handle();
        let ext: Option<PluginLatency> = handle.get_extension();
        ext.map(|l| l.get(&handle)).unwrap_or(0)
    };
    let (params_tx, params_rx) = mailbox::channel(PARAM_RING);
    let (reported_tx, reported_rx) = mailbox::channel(REPORT_RING);
    let processor = ClapProcessor::new(plugin, stopped.into(), &ins, &outs, block as usize, params_rx, reported_tx);
    let has_editor = editor::has_editor(&mut instance);
    let editor_open = Arc::new(AtomicBool::new(false));
    plugins.insert(
        plugin,
        Slot {
            instance,
            callback,
            name: info.name.clone(),
            gui,
            editor: None,
            editor_open: editor_open.clone(),
            linked: true,
            processing: true,
        },
    );
    Ok(Loaded { plugin, info, latency, has_editor, editor_open, params, processor, params_tx, reported_rx })
}

/// Channel counts of the plugin's input and output ports.
fn ports(instance: &mut PluginInstance<Host>) -> (Vec<u32>, Vec<u32>) {
    let handle = instance.plugin_handle();
    let Some(ext) = handle.get_extension::<PluginAudioPorts>() else { return (Vec::new(), Vec::new()) };
    let side = |is_input: bool| {
        let mut buf = AudioPortInfoBuffer::default();
        (0..ext.count(&handle, is_input))
            .filter_map(|i| ext.get(&handle, i, is_input, &mut buf).map(|p| p.channel_count))
            .collect::<Vec<u32>>()
    };
    (side(true), side(false))
}

fn list_params(instance: &mut PluginInstance<Host>) -> Vec<ParamState> {
    let mut out = Vec::new();
    let handle = instance.plugin_handle();
    let Some(ext) = handle.get_extension::<PluginParams>() else { return out };
    let mut buf = ParamInfoBuffer::new();
    for i in 0..ext.count(&handle) {
        let Some(p) = ext.get_info(&handle, i, &mut buf) else { continue };
        if p.flags.contains(ParamInfoFlags::IS_HIDDEN) {
            continue;
        }
        let id = p.id.get();
        let value = ext.get_value(&handle, p.id).unwrap_or(p.default_value);
        let mut text_buf = [0u8; 128];
        let text = ext
            .value_to_text(&handle, p.id, value, &mut text_buf)
            .ok()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_else(|| format!("{value:.3}"));
        out.push(ParamState {
            id,
            name: String::from_utf8_lossy(p.name).into_owned(),
            module: String::from_utf8_lossy(p.module).into_owned(),
            min: p.min_value,
            max: p.max_value,
            default: p.default_value,
            value,
            text,
            stepped: p.flags.contains(ParamInfoFlags::IS_STEPPED),
            read_only: p.flags.contains(ParamInfoFlags::IS_READONLY),
        });
    }
    out
}

fn text(instance: &mut PluginInstance<Host>, id: u32, value: f64) -> Option<String> {
    let handle = instance.plugin_handle();
    let ext = handle.get_extension::<PluginParams>()?;
    let mut buf = [0u8; 128];
    ext.value_to_text(&handle, ClapId::new(id), value, &mut buf).ok().map(|b| String::from_utf8_lossy(b).into_owned())
}

fn values(instance: &mut PluginInstance<Host>) -> Vec<(u32, f64)> {
    let handle = instance.plugin_handle();
    let Some(ext) = handle.get_extension::<PluginParams>() else { return Vec::new() };
    let mut buf = ParamInfoBuffer::new();
    (0..ext.count(&handle))
        .filter_map(|i| {
            let id = ext.get_info(&handle, i, &mut buf)?.id;
            Some((id.get(), ext.get_value(&handle, id)?))
        })
        .collect()
}

fn save_state(instance: &mut PluginInstance<Host>) -> Result<Vec<u8>, String> {
    let handle = instance.plugin_handle();
    let ext = handle.get_extension::<PluginState>().ok_or("this plugin cannot save its state")?;
    let mut out = Vec::new();
    ext.save(&handle, &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn load_state(instance: &mut PluginInstance<Host>, state: &[u8]) -> Result<(), String> {
    let handle = instance.plugin_handle();
    let ext = handle.get_extension::<PluginState>().ok_or("this plugin cannot load a state")?;
    let mut reader = state;
    ext.load(&handle, &mut reader).map_err(|e| e.to_string())
}
