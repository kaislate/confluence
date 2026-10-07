//! Control API over a Windows named pipe (`\\.\pipe\<name>`), restricted to
//! the current user and SYSTEM, local clients only. One thread per connection.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use confluence_api::{read_envelope, write_frame, Command, Envelope, Event, FrameError, Response, State};
pub use confluence_client::{default_pipe_name, pipe_path};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// What a pipe server offers: commands, and optionally subscriptions. Called
/// concurrently from connection threads.
pub trait Service: Send + Sync {
    fn handle(&self, cmd: &Command) -> Response;
    /// A snapshot and its event stream, or `None` if this server has none.
    fn subscribe(&self) -> Option<(State, Receiver<Event>)>;
}

pub type Handler = Arc<dyn Service>;

struct FnService<F>(F);

impl<F: Fn(&Command) -> Response + Send + Sync> Service for FnService<F> {
    fn handle(&self, cmd: &Command) -> Response {
        (self.0)(cmd)
    }

    fn subscribe(&self) -> Option<(State, Receiver<Event>)> {
        None
    }
}

/// A command-only service from a closure.
pub fn service_fn<F: Fn(&Command) -> Response + Send + Sync + 'static>(f: F) -> Handler {
    Arc::new(FnService(f))
}

const BUFFER_BYTES: u32 = 64 * 1024;

/// Owned security descriptor granting full access to SYSTEM and the current user only.
struct UserOnlySecurity(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is heap memory owned exclusively by this value.
unsafe impl Send for UserOnlySecurity {}

/// A pipe handle moved to the accept thread, which becomes its only user.
struct OwnedPipe(HANDLE);

// SAFETY: kernel handles may be used from any thread; ownership is unique.
unsafe impl Send for OwnedPipe {}

impl OwnedPipe {
    /// Releases ownership of the handle to the caller.
    fn into_handle(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for OwnedPipe {
    fn drop(&mut self) {
        // SAFETY: we own the handle and nothing else closes it.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl UserOnlySecurity {
    fn new() -> io::Result<Self> {
        let sid = current_user_sid()?;
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: valid SDDL string and out-pointer; freed with LocalFree in Drop.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut psd, None)? };
        Ok(Self(psd))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for UserOnlySecurity {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(Some(HLOCAL(self.0 .0))) };
    }
}

fn current_user_sid() -> io::Result<String> {
    // SAFETY: standard token query; buffers sized from the first call; handles closed.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        // u64 storage keeps TOKEN_USER suitably aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let res = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len);
        let _ = CloseHandle(token);
        res?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s)?;
        let out = s.to_string().map_err(io::Error::other);
        LocalFree(Some(HLOCAL(s.0.cast())));
        out
    }
}

pub struct PipeServer {
    path: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// A pipe name claimed by this process but not yet serving. Claiming first lets
/// the engine prove it is the only instance before it touches any state.
pub struct PipeListener {
    path: String,
    first: OwnedPipe,
    security: UserOnlySecurity,
}

impl PipeListener {
    /// Starts accepting connections with `handler`.
    pub fn serve(self, handler: Handler) -> io::Result<PipeServer> {
        let Self { path, first, security } = self;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::Builder::new()
                .name("confluence-pipe".into())
                .spawn(move || accept_loop(&path, first, security, &stop, handler))?
        };
        Ok(PipeServer { path, stop, thread: Some(thread) })
    }
}

impl PipeServer {
    /// Claims `\\.\pipe\<name>` for this process. Fails with
    /// `ErrorKind::AlreadyExists` if another engine already owns the name.
    pub fn bind(name: &str) -> io::Result<PipeListener> {
        let path = pipe_path(name);
        let security = UserOnlySecurity::new()?;
        let first = create_instance(&path, &security, true).map_err(|e| match e.raw_os_error() {
            // FILE_FLAG_FIRST_PIPE_INSTANCE reports an existing owner as access denied (or busy).
            Some(5) | Some(231) => {
                io::Error::new(io::ErrorKind::AlreadyExists, format!("another engine is already running on {path}"))
            }
            _ => e,
        })?;
        Ok(PipeListener { path, first: OwnedPipe(first), security })
    }

    /// Claims the name and starts serving in one step.
    pub fn start(name: &str, handler: Handler) -> io::Result<Self> {
        Self::bind(name)?.serve(handler)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop blocked in ConnectNamedPipe.
        let _ = OpenOptions::new().read(true).write(true).open(&self.path);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn create_instance(path: &str, security: &UserOnlySecurity, first: bool) -> io::Result<HANDLE> {
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let sa = security.attributes();
    // SAFETY: valid name and security attributes that outlive the call.
    let h = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(path),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_BYTES,
            BUFFER_BYTES,
            0,
            Some(&sa),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(h)
}

fn accept_loop(path: &str, first: OwnedPipe, security: UserOnlySecurity, stop: &AtomicBool, handler: Handler) {
    let mut next = Some(first.into_handle());
    while !stop.load(Ordering::SeqCst) {
        let h = match next.take() {
            Some(h) => h,
            None => match create_instance(path, &security, false) {
                Ok(h) => h,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            },
        };
        // SAFETY: `h` is a valid pipe handle; blocking connect.
        let connected = unsafe { ConnectNamedPipe(h, None) };
        let ok = match connected {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if !ok || stop.load(Ordering::SeqCst) {
            // SAFETY: we own `h`.
            let _ = unsafe { CloseHandle(h) };
            continue;
        }
        // SAFETY: ownership of the handle moves into the File.
        let file = unsafe { File::from_raw_handle(h.0) };
        let handler = handler.clone();
        let _ = std::thread::Builder::new().name("confluence-pipe-client".into()).spawn(move || serve(file, handler));
    }
}

fn serve(mut file: File, handler: Handler) {
    loop {
        match read_envelope::<_, Command>(&mut file) {
            Ok(Some(env)) if env.body == Command::Subscribe => {
                stream(file, env.id, handler);
                return;
            }
            Ok(Some(env)) => {
                let resp = handler.handle(&env.body);
                if write_frame(&mut file, &Envelope::new(env.id, resp)).is_err() {
                    return;
                }
            }
            Err(FrameError::Version(v)) => {
                let msg = Response::Error(format!("unsupported protocol version {v}"));
                let _ = write_frame(&mut file, &Envelope::new(0, msg));
                return;
            }
            Ok(None) | Err(_) => return,
        }
    }
}

/// Writes the snapshot, then every event, until the client or the engine goes
/// away. After `Subscribe` nothing more is read from the connection.
fn stream(mut file: File, id: u32, handler: Handler) {
    let subscription = handler.subscribe();
    // A subscriber that stops reading can block a write here for good: it
    // must not keep the service (and the engine's state) alive meanwhile.
    drop(handler);
    let Some((snapshot, events)) = subscription else {
        let msg = Response::Error("subscriptions are not available".into());
        let _ = write_frame(&mut file, &Envelope::new(id, msg));
        return;
    };
    if write_frame(&mut file, &Envelope::new(id, Response::Snapshot(snapshot))).is_err() {
        return;
    }
    // The receiver ends when the publisher drops this subscriber (queue full)
    // or the engine shuts down; a write fails when the client has gone.
    while let Ok(event) = events.recv() {
        if write_frame(&mut file, &Envelope::new(0, Response::Event(event))).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn unique_name(tag: &str) -> String {
        format!("confluence-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn commands_round_trip_over_the_pipe() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = {
            let seen = seen.clone();
            service_fn(move |cmd: &Command| {
                seen.lock().unwrap().push(cmd.clone());
                Response::Ok
            })
        };
        let name = unique_name("roundtrip");
        let server = PipeServer::start(&name, handler).unwrap();
        let mut a = confluence_client::Client::connect(&name, Duration::from_secs(2)).unwrap();
        let mut b = confluence_client::Client::connect(&name, Duration::from_secs(2)).unwrap();
        assert_eq!(a.call(Command::ListPoints).unwrap(), Response::Ok);
        assert_eq!(b.call(Command::Health).unwrap(), Response::Ok);
        assert_eq!(a.call(Command::ListSlots).unwrap(), Response::Ok);
        server.stop();
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn second_server_on_same_name_is_refused() {
        let name = unique_name("clash");
        let handler = service_fn(|_: &Command| Response::Ok);
        let _server = PipeServer::start(&name, handler.clone()).unwrap();
        let err = PipeServer::start(&name, handler).err().expect("name is taken");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("already running"), "{err}");
    }

    #[test]
    fn a_bound_listener_holds_the_name_until_dropped() {
        let name = unique_name("bind");
        let listener = PipeServer::bind(&name).unwrap();
        assert!(PipeServer::bind(&name).is_err(), "claimed before serving");
        drop(listener);
        let handler = service_fn(|_: &Command| Response::Ok);
        let _server = PipeServer::start(&name, handler).unwrap();
    }

    #[test]
    fn hostile_client_is_dropped_and_others_keep_working() {
        use std::io::{Read, Write};
        let name = unique_name("hostile");
        let handler = service_fn(|_: &Command| Response::Ok);
        let _server = PipeServer::start(&name, handler).unwrap();
        let mut raw = OpenOptions::new().read(true).write(true).open(pipe_path(&name)).unwrap();
        raw.write_all(&u32::MAX.to_le_bytes()).unwrap();
        let mut byte = [0u8; 1];
        assert!(!matches!(raw.read(&mut byte), Ok(1)), "server hung up instead of answering");
        let mut good = confluence_client::Client::connect(&name, Duration::from_secs(2)).unwrap();
        assert_eq!(good.call(Command::Health).unwrap(), Response::Ok);
    }

    struct Streaming {
        publisher: Mutex<crate::publish::Publisher>,
    }

    impl Service for Streaming {
        fn handle(&self, cmd: &Command) -> Response {
            match cmd {
                Command::ListPoints => Response::Points(Vec::new()),
                _ => Response::Error("unsupported".into()),
            }
        }

        fn subscribe(&self) -> Option<(State, Receiver<Event>)> {
            Some(self.publisher.lock().unwrap().subscribe())
        }
    }

    fn empty_state() -> State {
        State {
            version: 0,
            status: confluence_api::EngineStatus {
                master: "internal".into(),
                sample_rate: 48_000.0,
                block: 256,
                blocks: 0,
                dsp_load: 0.0,
                xruns: 0,
            },
            slots: Vec::new(),
            points: Vec::new(),
            devices: Vec::new(),
            notices: Vec::new(),
            plugins: Vec::new(),
            bad_plugins: Vec::new(),
            bus_plugins: Vec::new(),
            scenes: Vec::new(),
            current_scene: None,
            morphing: false,
            midi_inputs: Vec::new(),
            midi_bindings: Vec::new(),
            midi_learning: None,
            scripts: Vec::new(),
            peers: Vec::new(),
            positions: Vec::new(),
        }
    }

    #[test]
    fn a_subscription_streams_events_while_commands_still_work() {
        let name = unique_name("sub");
        let svc = Arc::new(Streaming { publisher: Mutex::new(crate::publish::Publisher::new(empty_state())) });
        let server = PipeServer::start(&name, svc.clone()).unwrap();
        let (snap, mut sub) = confluence_client::Subscription::connect(&name, Duration::from_secs(5)).unwrap();
        assert_eq!(snap.version, 0);
        let mut next = empty_state();
        next.notices = vec!["hello".into()];
        svc.publisher.lock().unwrap().publish(next);
        match sub.recv().unwrap() {
            Event::Changed { version: 1, changes } => assert_eq!(changes.len(), 1),
            other => panic!("{other:?}"),
        }
        let mut c = confluence_client::Client::connect(&name, Duration::from_secs(5)).unwrap();
        assert_eq!(c.call(Command::ListPoints).unwrap(), Response::Points(Vec::new()));
        server.stop();
    }

    #[test]
    fn a_server_without_subscriptions_refuses_them() {
        let name = unique_name("nosub");
        let server = PipeServer::start(&name, service_fn(|_| Response::Ok)).unwrap();
        let err = confluence_client::Subscription::connect(&name, Duration::from_secs(5)).err().unwrap();
        assert!(matches!(err, confluence_client::ClientError::Refused(_)), "{err:?}");
        server.stop();
    }
}
