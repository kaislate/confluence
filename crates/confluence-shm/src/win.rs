//! Named mappings and events in the session's `Local\` namespace.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_ALL_ACCESS,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

use crate::ring::{ring, RingMemory, RingReader, RingWriter};
use crate::{Header, Layout, ShmError, MAGIC, VERSION};

const DIRECTORY_MAGIC: u64 = 0x5249_444D_4853_4643; // "CFSHMDIR"
const STATE_READY: u32 = 1;
const STATE_CLOSED: u32 = 2;

/// Fixed-size record naming a base name's current generation.
#[repr(C)]
struct Directory {
    magic: AtomicU64,
    generation: AtomicU64,
    state: AtomicU32,
    _reserved: u32,
}

fn err(call: &'static str) -> impl Fn(windows::core::Error) -> ShmError {
    move |e| ShmError::Win32 { call, message: e.message() }
}

fn names(base: &str, generation: u64) -> (HSTRING, HSTRING) {
    (
        HSTRING::from(format!("Local\\Confluence.{base}.{generation:016x}")),
        HSTRING::from(format!("Local\\Confluence.{base}.{generation:016x}.wake")),
    )
}

fn directory_name(base: &str) -> HSTRING {
    HSTRING::from(format!("Local\\Confluence.{base}"))
}

/// A mapped view of a named mapping; unmapped and closed on drop.
struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    size: usize,
}

// SAFETY: the view is plain shared memory; access is coordinated by atomics.
unsafe impl Send for Mapping {}

impl Mapping {
    /// Creates (or opens, if it exists) a mapping of `size` bytes. Returns
    /// whether it already existed.
    fn create(name: &HSTRING, size: usize) -> Result<(Self, bool), ShmError> {
        // SAFETY: page-file backed mapping; the name is a valid wide string.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (size as u64 >> 32) as u32,
                size as u32,
                name,
            )
        }
        .map_err(err("CreateFileMapping"))?;
        // SAFETY: reads the calling thread's last error, set by the call above.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((Self::map(handle)?, existed))
    }

    fn open(name: &HSTRING) -> Result<Self, ShmError> {
        // SAFETY: valid wide string.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, name) }.map_err(err("OpenFileMapping"))?;
        Self::map(handle)
    }

    fn map(handle: HANDLE) -> Result<Self, ShmError> {
        // SAFETY: `handle` is a live mapping handle; size 0 maps all of it.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            // SAFETY: we own the handle.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(ShmError::Win32 {
                call: "MapViewOfFile",
                message: windows::core::Error::from_thread().message(),
            });
        }
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: queries our own view.
        unsafe { VirtualQuery(Some(view.Value), &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        Ok(Mapping { handle, view, size: info.RegionSize })
    }

    fn ptr(&self) -> *mut u8 {
        self.view.Value.cast()
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped and opened by us.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A named auto-reset event.
struct Event(HANDLE);

// SAFETY: kernel event handles may be used from any thread.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn create(name: &HSTRING) -> Result<Self, ShmError> {
        // SAFETY: valid wide string; auto-reset, initially unsignalled.
        unsafe { CreateEventW(None, false, false, name) }.map(Event).map_err(err("CreateEvent"))
    }

    fn open(name: &HSTRING) -> Result<Self, ShmError> {
        // SAFETY: valid wide string.
        unsafe { OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, false, name) }
            .map(Event)
            .map_err(err("OpenEvent"))
    }

    fn set(&self) {
        // SAFETY: live event handle.
        unsafe {
            let _ = SetEvent(self.0);
        }
    }

    /// True if signalled within `timeout_ms`.
    fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: live event handle.
        unsafe { WaitForSingleObject(self.0, timeout_ms) }.0 == 0
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: opened or created by us.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A stream's mapping, viewed as its header and two rings.
struct Stream {
    map: Mapping,
    event: Event,
}

impl Stream {
    fn header(&self) -> &Header {
        // SAFETY: the mapping is at least a header long (checked on create/connect)
        // and page-aligned.
        unsafe { &*self.map.ptr().cast::<Header>() }
    }

    fn memories(&self) -> (RingMemory, RingMemory) {
        let h = self.header();
        let base = self.map.ptr();
        let to_samples = Layout::samples_offset();
        let from_samples =
            to_samples + (h.to_client_channels * h.capacity_frames) as usize * std::mem::size_of::<f32>();
        (
            RingMemory {
                counters: &h.to_client,
                // SAFETY: inside the mapping (layout checked against its size).
                samples: unsafe { base.add(to_samples) }.cast(),
                capacity: h.capacity_frames as u64,
                channels: h.to_client_channels as usize,
            },
            RingMemory {
                counters: &h.from_client,
                // SAFETY: as above.
                samples: unsafe { base.add(from_samples) }.cast(),
                capacity: h.capacity_frames as u64,
                channels: h.from_client_channels as usize,
            },
        )
    }
}

/// The publishing end of a stream (the engine). Dropping it marks the stream
/// closed, so clients fall back and wait for the next generation.
pub struct Server {
    directory: Mapping,
    stream: Stream,
    generation: u64,
}

impl Server {
    /// Publishes a new generation of the stream `base` with `layout`.
    pub fn create(base: &str, layout: Layout) -> Result<Self, ShmError> {
        if !layout.is_sane() {
            return Err(ShmError::Layout(layout));
        }
        let (directory, _) = Mapping::create(&directory_name(base), std::mem::size_of::<Directory>())?;
        // SAFETY: page-aligned and at least a Directory long.
        let dir = unsafe { &*directory.ptr().cast::<Directory>() };
        let previous = dir.generation.load(Ordering::Acquire);
        let mut generation = (u64::from(std::process::id()) << 32)
            | u64::from(std::time::SystemTime::UNIX_EPOCH.elapsed().map_or(0, |d| d.subsec_nanos()));
        if generation == previous {
            generation = generation.wrapping_add(1);
        }
        let (map_name, event_name) = names(base, generation);
        let (map, existed) = Mapping::create(&map_name, layout.bytes())?;
        if existed || map.size < layout.bytes() {
            return Err(ShmError::Malformed);
        }
        let event = Event::create(&event_name)?;
        // SAFETY: fresh, zeroed, page-aligned mapping of `layout.bytes()`; no
        // client can see it until the directory publishes this generation.
        unsafe {
            let h = map.ptr().cast::<Header>();
            (*h).magic = MAGIC;
            (*h).version = VERSION;
            (*h).block = layout.block;
            (*h).sample_rate = layout.sample_rate;
            (*h).to_client_channels = layout.to_client_channels;
            (*h).from_client_channels = layout.from_client_channels;
            (*h).capacity_frames = layout.capacity_frames;
        }
        dir.magic.store(DIRECTORY_MAGIC, Ordering::Relaxed);
        dir.generation.store(generation, Ordering::Release);
        dir.state.store(STATE_READY, Ordering::Release);
        Ok(Server { directory, stream: Stream { map, event }, generation })
    }

    pub fn header(&self) -> &Header {
        self.stream.header()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The server's ends: it writes to the client and reads from it. Call once.
    pub fn ends(&self) -> (RingWriter, RingReader) {
        let (to, from) = self.stream.memories();
        // SAFETY: the rings live in `self`'s mapping; the caller keeps the
        // server alive while using them (enforced by `ServerEnds` in callers).
        let ((writer, _), (_, reader)) = unsafe { (ring(to), ring(from)) };
        (writer, reader)
    }

    /// Wakes the client after a block has been written.
    pub fn wake_client(&self) {
        self.stream.event.set();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // SAFETY: page-aligned and at least a Directory long.
        let dir = unsafe { &*self.directory.ptr().cast::<Directory>() };
        if dir.generation.load(Ordering::Acquire) == self.generation {
            dir.state.store(STATE_CLOSED, Ordering::Release);
        }
    }
}

/// The connecting end of a stream (e.g. a DAW's VASIO driver).
pub struct Client {
    directory: Mapping,
    stream: Stream,
    generation: u64,
}

impl Client {
    /// Connects to the current generation of `base`. `Ok(None)` if no server
    /// is publishing it.
    pub fn connect(base: &str) -> Result<Option<Self>, ShmError> {
        let Ok(directory) = Mapping::open(&directory_name(base)) else { return Ok(None) };
        if directory.size < std::mem::size_of::<Directory>() {
            return Err(ShmError::Malformed);
        }
        // SAFETY: page-aligned and at least a Directory long (checked).
        let dir = unsafe { &*directory.ptr().cast::<Directory>() };
        if dir.state.load(Ordering::Acquire) != STATE_READY || dir.magic.load(Ordering::Relaxed) != DIRECTORY_MAGIC {
            return Ok(None);
        }
        let generation = dir.generation.load(Ordering::Acquire);
        let (map_name, event_name) = names(base, generation);
        let (Ok(map), Ok(event)) = (Mapping::open(&map_name), Event::open(&event_name)) else { return Ok(None) };
        if map.size < std::mem::size_of::<Header>() {
            return Err(ShmError::Malformed);
        }
        let stream = Stream { map, event };
        let h = stream.header();
        if h.magic != MAGIC || h.version != VERSION || !h.layout().is_sane() || stream.map.size < h.layout().bytes() {
            return Err(ShmError::Malformed);
        }
        Ok(Some(Client { directory, stream, generation }))
    }

    pub fn header(&self) -> &Header {
        self.stream.header()
    }

    /// The server generation this client is connected to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// False once the server has closed or been replaced by a new generation.
    pub fn is_current(&self) -> bool {
        // SAFETY: checked at connect.
        let dir = unsafe { &*self.directory.ptr().cast::<Directory>() };
        dir.state.load(Ordering::Acquire) == STATE_READY && dir.generation.load(Ordering::Acquire) == self.generation
    }

    /// The client's ends: it reads from the server and writes to it. Call once.
    pub fn ends(&self) -> (RingReader, RingWriter) {
        let (to, from) = self.stream.memories();
        // SAFETY: as for `Server::ends`.
        let ((_, reader), (writer, _)) = unsafe { (ring(to), ring(from)) };
        (reader, writer)
    }

    /// Waits up to `timeout_ms` for the server's next block; true if it came.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        self.stream.event.wait(timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            sample_rate: 48_000.0,
            block: 64,
            to_client_channels: 2,
            from_client_channels: 3,
            capacity_frames: 256,
        }
    }

    fn base(tag: &str) -> String {
        format!("test.{tag}.{}", std::process::id())
    }

    #[test]
    fn a_client_sees_the_servers_layout_and_audio_both_ways() {
        let name = base("both-ways");
        let server = Server::create(&name, layout()).unwrap();
        let client = Client::connect(&name).unwrap().expect("server is publishing");
        assert_eq!(client.header().layout(), layout());
        let (mut to_client, mut from_client) = server.ends();
        let (mut reader, mut writer) = client.ends();
        assert!(to_client.write_frames(64, |ch, f| ch as f32 + f as f32 / 100.0));
        server.wake_client();
        assert!(client.wait(1000), "the wake-up event crosses to the client");
        assert!(reader.read_frames(64, |ch, f, s| assert_eq!(s, ch as f32 + f as f32 / 100.0)));
        assert!(writer.write_frames(64, |ch, _| -(ch as f32)));
        assert!(from_client.read_frames(64, |ch, _, s| assert_eq!(s, -(ch as f32))));
    }

    #[test]
    fn no_server_means_no_connection_not_an_error() {
        assert!(Client::connect(&base("nobody")).unwrap().is_none());
    }

    #[test]
    fn a_restarted_server_is_a_new_generation_the_client_notices() {
        let name = base("restart");
        let first = Server::create(&name, layout()).unwrap();
        let client = Client::connect(&name).unwrap().unwrap();
        assert!(client.is_current());
        drop(first);
        assert!(!client.is_current(), "closing the server is visible");
        assert!(Client::connect(&name).unwrap().is_none(), "nothing to connect to while closed");
        let bigger = Layout { to_client_channels: 16, ..layout() };
        let _second = Server::create(&name, bigger).unwrap();
        assert!(!client.is_current());
        let again = Client::connect(&name).unwrap().unwrap();
        assert_eq!(again.header().layout(), bigger, "a new generation may have a different size");
        // The old client's view stays valid (no crash) even though it is stale.
        assert_eq!(client.header().layout(), layout());
    }

    #[test]
    fn nonsense_layouts_are_refused() {
        let bad = Layout { block: 0, ..layout() };
        assert_eq!(Server::create(&base("bad"), bad).err(), Some(ShmError::Layout(bad)));
    }
}
