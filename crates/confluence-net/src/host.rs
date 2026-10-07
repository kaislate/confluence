//! The network host: one UDP socket and one thread per engine. Received
//! packets go through each added stream's [`Receiver`] to its slot; audio from
//! send slots is packetized and sent. Streams nobody added are only listed.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use confluence_api::NetStats;
use confluence_core::bridge::InputDeviceSide;
use confluence_core::buffer::PlanarBuffer;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::packet::{self, Format, Header};
use crate::receiver::{Receiver, ReceiverStats};

/// A stream not heard for this long drops off the list.
const HEARD_FOR: Duration = Duration::from_secs(5);
/// Most streams listed (a flood of names cannot grow it further).
const MAX_HEARD: usize = 64;
/// The socket thread wakes at least this often (concealment, sending).
const TICK: Duration = Duration::from_millis(1);
/// Socket buffers: a stalled network thread (a busy PC) loses nothing for
/// tens of milliseconds even on wide streams.
const SOCKET_BUFFER: i32 = 1 << 20;
/// Frames a send slot may queue before the network thread takes them.
const SEND_RING_FRAMES: usize = 16_384;

/// Where a receive stream's audio goes: the slot's bridge (or a test tap).
pub trait FrameSink: Send {
    fn write(&mut self, data: &[f32], time: f64);
    /// The receiver may hold audio back this many frames (waiting for a late packet).
    fn set_latency_floor(&self, frames: f64);
    /// The times written from now on are on another base (see
    /// `InputDeviceSide::restart_clock`).
    fn restart_clock(&mut self) {}
}

impl FrameSink for InputDeviceSide {
    fn write(&mut self, data: &[f32], time: f64) {
        self.write_interleaved(data, time);
    }
    fn set_latency_floor(&self, frames: f64) {
        InputDeviceSide::set_latency_floor(self, frames);
    }
    fn restart_clock(&mut self) {
        InputDeviceSide::restart_clock(self);
    }
}

/// A stream being received or not, as last heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    pub from: IpAddr,
    pub stream: String,
    pub channels: u8,
    pub rate: u32,
}

pub struct SendSpec {
    pub dest: SocketAddr,
    pub stream: String,
    pub channels: usize,
    pub rate: u32,
}

#[derive(Default)]
struct RecvShared {
    stats: ReceiverStats,
    malformed: u64,
    last_packet: Option<Instant>,
}

struct RecvEntry {
    id: u64,
    from: IpAddr,
    stream: String,
    receiver: Receiver,
    sink: Box<dyn FrameSink>,
    shared: Arc<Mutex<RecvShared>>,
}

struct SendEntry {
    id: u64,
    spec: SendSpec,
    audio: Consumer<f32>,
    ssrc: u32,
    seq: u16,
    timestamp: u32,
    block: Vec<f32>,
    part: Vec<f32>,
    datagram: Vec<u8>,
    packets: Arc<AtomicU64>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    receivers: Vec<RecvEntry>,
    senders: Vec<SendEntry>,
    heard: HashMap<(IpAddr, String), (u8, u32, Instant)>,
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// FNV-1a: the stream's RTP SSRC, from the engine id and stream name.
fn ssrc(engine_id: u64, stream: &str) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in engine_id.to_le_bytes().iter().chain(stream.as_bytes()) {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The engine's network host. Dropping it stops the thread.
pub struct NetHost {
    port: u16,
    engine_id: u64,
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    socket: UdpSocket,
}

#[cfg(windows)]
fn socket_option(socket: &UdpSocket, option: i32, value: Option<i32>) -> i32 {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{getsockopt, setsockopt, SOCKET, SOL_SOCKET};
    let s = SOCKET(socket.as_raw_socket() as usize);
    // SAFETY: a live socket we own; option buffers are i32-sized.
    unsafe {
        if let Some(v) = value {
            setsockopt(s, SOL_SOCKET, option, Some(&v.to_ne_bytes()));
        }
        let mut out = [0u8; 4];
        let mut len = 4i32;
        getsockopt(s, SOL_SOCKET, option, windows::core::PSTR(out.as_mut_ptr()), &mut len);
        i32::from_ne_bytes(out)
    }
}

impl NetHost {
    /// Binds `bind` (port 0: any free port) and starts the network thread.
    pub fn start(bind: SocketAddr, engine_id: u64) -> std::io::Result<NetHost> {
        let socket = UdpSocket::bind(bind)?;
        // Non-blocking, waited on with a poll: a receive timeout
        // (SO_RCVTIMEO) leaves a Windows socket in an undefined state.
        socket.set_nonblocking(true)?;
        #[cfg(windows)]
        {
            use windows::Win32::Networking::WinSock::{SO_RCVBUF, SO_SNDBUF};
            socket_option(&socket, SO_RCVBUF, Some(SOCKET_BUFFER));
            socket_option(&socket, SO_SNDBUF, Some(SOCKET_BUFFER));
        }
        let query = socket.try_clone()?;
        let port = socket.local_addr()?.port();
        let inner = Arc::new(Mutex::new(Inner::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (i, s) = (inner.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("confluence-net".into()).spawn(move || run(socket, i, s))?;
        Ok(NetHost { port, engine_id, inner, stop, thread: Some(thread), socket: query })
    }

    /// The socket's receive buffer, in bytes.
    pub fn receive_buffer_bytes(&self) -> usize {
        #[cfg(windows)]
        {
            socket_option(&self.socket, windows::Win32::Networking::WinSock::SO_RCVBUF, None).max(0) as usize
        }
        #[cfg(not(windows))]
        {
            let _ = &self.socket;
            SOCKET_BUFFER as usize
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Plays stream `stream` from `from` into `sink` (`channels` wide). The
    /// stream stops when the handle is dropped.
    pub fn add_receiver(
        &self,
        from: IpAddr,
        stream: &str,
        channels: usize,
        rate: u32,
        sink: Box<dyn FrameSink>,
    ) -> ReceiveHandle {
        let shared = Arc::new(Mutex::new(RecvShared::default()));
        let mut g = lock(&self.inner);
        g.next_id += 1;
        let id = g.next_id;
        g.receivers.push(RecvEntry {
            id,
            from,
            stream: stream.to_string(),
            receiver: Receiver::new(channels, rate),
            sink,
            shared: shared.clone(),
        });
        ReceiveHandle { id, inner: self.inner.clone(), shared }
    }

    /// Sends what is written to the returned side to `spec.dest`, until the handle is dropped.
    pub fn add_sender(&self, spec: SendSpec) -> (SendSide, SendHandle) {
        let channels = spec.channels.max(1);
        let (audio_in, audio) = RingBuffer::new(SEND_RING_FRAMES * channels);
        let dropped = Arc::new(AtomicU64::new(0));
        let packets = Arc::new(AtomicU64::new(0));
        let mut g = lock(&self.inner);
        g.next_id += 1;
        let id = g.next_id;
        let frames = (spec.rate / 1000).max(1) as usize;
        g.senders.push(SendEntry {
            id,
            ssrc: ssrc(self.engine_id, &spec.stream),
            spec,
            audio,
            seq: 0,
            timestamp: 0,
            block: vec![0.0; frames * channels],
            part: Vec::with_capacity(frames * channels),
            datagram: Vec::with_capacity(packet::MAX_DATAGRAM),
            packets: packets.clone(),
        });
        let side =
            SendSide { audio: audio_in, channels, scratch: vec![0.0; 8192 * channels], dropped: dropped.clone() };
        (side, SendHandle { id, inner: self.inner.clone(), packets, dropped })
    }

    /// Streams heard in the last few seconds, added or not.
    pub fn heard(&self) -> Vec<Heard> {
        let g = lock(&self.inner);
        let mut v: Vec<Heard> = g
            .heard
            .iter()
            .filter(|(_, (_, _, at))| at.elapsed() < HEARD_FOR)
            .map(|((from, stream), (channels, rate, _))| Heard {
                from: *from,
                stream: stream.clone(),
                channels: *channels,
                rate: *rate,
            })
            .collect();
        v.sort_by(|a, b| (a.from, &a.stream).cmp(&(b.from, &b.stream)));
        v
    }
}

impl Drop for NetHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A receive stream; dropping it removes the stream.
pub struct ReceiveHandle {
    id: u64,
    inner: Arc<Mutex<Inner>>,
    shared: Arc<Mutex<RecvShared>>,
}

impl ReceiveHandle {
    pub fn stats(&self) -> NetStats {
        let s = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        NetStats {
            packets: s.stats.packets,
            lost: s.stats.lost,
            late: s.stats.late,
            reordered: s.stats.reordered,
            malformed: s.malformed,
            mismatched: s.stats.mismatched,
            silent_ms: s.last_packet.map_or(u64::MAX, |t| t.elapsed().as_millis() as u64),
        }
    }
}

impl Drop for ReceiveHandle {
    fn drop(&mut self) {
        lock(&self.inner).receivers.retain(|r| r.id != self.id);
    }
}

/// The audio-thread end of a send stream.
pub struct SendSide {
    audio: Producer<f32>,
    channels: usize,
    scratch: Vec<f32>,
    dropped: Arc<AtomicU64>,
}

impl SendSide {
    /// Queues `block`'s channels `first..first + channels` (no allocation).
    pub fn write(&mut self, block: &PlanarBuffer, first: usize) {
        let c = self.channels;
        let frames = block.frames().min(self.scratch.len() / c);
        for ch in 0..c {
            let src = if first + ch < block.channels() { Some(block.channel(first + ch)) } else { None };
            for f in 0..frames {
                self.scratch[f * c + ch] = src.map_or(0.0, |s| s[f]);
            }
        }
        if self.audio.push_entire_slice(&self.scratch[..frames * c]).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A send stream; dropping it stops sending.
pub struct SendHandle {
    id: u64,
    inner: Arc<Mutex<Inner>>,
    packets: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl SendHandle {
    pub fn packets(&self) -> u64 {
        self.packets.load(Ordering::Relaxed)
    }
    /// Blocks the network thread could not keep up with.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for SendHandle {
    fn drop(&mut self) {
        lock(&self.inner).senders.retain(|s| s.id != self.id);
    }
}

/// Asks Windows for 1 ms timer resolution while it lives: on the default
/// 15.6 ms timer the thread would send and read packets in clumps (receivers
/// cope, at the cost of latency).
struct FineTimer;

impl FineTimer {
    fn start() -> FineTimer {
        #[cfg(windows)]
        // SAFETY: plain call; undone in Drop.
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        FineTimer
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: undoes the call in `start`.
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

/// Waits until the socket is readable or `TICK` has passed.
#[cfg(windows)]
fn wait_readable(socket: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, SOCKET, WSAPOLLFD};
    let mut fd =
        [WSAPOLLFD { fd: SOCKET(socket.as_raw_socket() as usize), events: POLLRDNORM, revents: Default::default() }];
    // SAFETY: one valid descriptor for a socket we own.
    unsafe { WSAPoll(fd.as_mut_ptr(), 1, TICK.as_millis() as i32) };
}

#[cfg(not(windows))]
fn wait_readable(_socket: &UdpSocket) {
    std::thread::sleep(TICK);
}

fn run(socket: UdpSocket, inner: Arc<Mutex<Inner>>, stop: Arc<AtomicBool>) {
    #[cfg(windows)]
    let _mmcss = confluence_rt::ProAudioThread::enter().ok();
    let _timer = FineTimer::start();
    let mut buf = [0u8; 2048];
    let mut samples = vec![0.0f32; 64 * 2048];
    while !stop.load(Ordering::Relaxed) {
        wait_readable(&socket);
        let mut g = lock(&inner);
        // Everything waiting, then the periodic work.
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let now = confluence_rt::now_seconds();
                    receive(&mut g, &buf[..n], from.ip(), now, &mut samples);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // Windows reports an ICMP "port unreachable" from an earlier send this way.
                Err(e) if e.kind() == ErrorKind::ConnectionReset => continue,
                Err(_) => break,
            }
        }
        let now = confluence_rt::now_seconds();
        for r in &mut g.receivers {
            let sink = &mut r.sink;
            r.receiver.poll(now, &mut |d, t| sink.write(d, t));
            if r.receiver.take_discontinuity() {
                r.sink.restart_clock();
            }
        }
        for s in &mut g.senders {
            send(&socket, s);
        }
    }
}

fn receive(g: &mut Inner, data: &[u8], from: IpAddr, now: f64, samples: &mut [f32]) {
    let p = match packet::parse(data) {
        Ok(p) => p,
        Err(_) => {
            for r in g.receivers.iter().filter(|r| r.from == from) {
                r.shared.lock().unwrap_or_else(|p| p.into_inner()).malformed += 1;
            }
            return;
        }
    };
    let h = &p.header;
    let key = (from, h.stream.clone());
    if g.heard.len() < MAX_HEARD || g.heard.contains_key(&key) {
        g.heard.insert(key, (h.total_channels, h.rate, Instant::now()));
    } else {
        g.heard.retain(|_, (_, _, at)| at.elapsed() < HEARD_FOR);
    }
    let Some(r) = g.receivers.iter_mut().find(|r| r.from == from && r.stream == h.stream) else { return };
    let n = p.frames() * h.channels as usize;
    p.decode(&mut samples[..n]);
    let sink = &mut r.sink;
    let refused = r.receiver.stats().mismatched;
    r.receiver.push(h, &samples[..n], now, &mut |d, t| sink.write(d, t));
    r.sink.set_latency_floor(r.receiver.latency_floor());
    if r.receiver.take_discontinuity() {
        r.sink.restart_clock();
    }
    let mut s = r.shared.lock().unwrap_or_else(|p| p.into_inner());
    s.stats = r.receiver.stats();
    // A packet this stream cannot play is no sign of life.
    if s.stats.mismatched == refused {
        s.last_packet = Some(Instant::now());
    }
}

fn send(socket: &UdpSocket, s: &mut SendEntry) {
    let c = s.spec.channels.max(1);
    let frames = s.block.len() / c;
    while s.audio.slots() >= s.block.len() {
        let Ok(chunk) = s.audio.read_chunk(s.block.len()) else { break };
        let (a, b) = chunk.as_slices();
        s.block[..a.len()].copy_from_slice(a);
        s.block[a.len()..].copy_from_slice(b);
        chunk.commit_all();
        for (first, count) in packet::split(c as u8, frames, Format::L24, &s.spec.stream) {
            s.part.clear();
            for f in 0..frames {
                let row = &s.block[f * c..(f + 1) * c];
                s.part.extend_from_slice(&row[first as usize..(first + count) as usize]);
            }
            let h = Header {
                seq: s.seq,
                timestamp: s.timestamp,
                ssrc: s.ssrc,
                format: Format::L24,
                total_channels: c as u8,
                first_channel: first,
                channels: count,
                rate: s.spec.rate,
                stream: s.spec.stream.clone(),
            };
            packet::build(&h, &s.part, &mut s.datagram);
            if socket.send_to(&s.datagram, s.spec.dest).is_ok() {
                s.packets.fetch_add(1, Ordering::Relaxed);
            }
            s.seq = s.seq.wrapping_add(1);
        }
        s.timestamp = s.timestamp.wrapping_add(frames as u32);
    }
}
