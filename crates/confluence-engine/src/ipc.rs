//! Control API over a Windows named pipe (`\\.\pipe\<name>`), restricted to
//! the current user and SYSTEM, local clients only. One thread per connection.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use confluence_api::{read_envelope, write_frame, Command, Envelope, FrameError, Response};
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

/// Handles one command; called concurrently from connection threads.
pub type Handler = Arc<dyn Fn(&Command) -> Response + Send + Sync>;

const BUFFER_BYTES: u32 = 64 * 1024;

pub fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

/// Default pipe name for the current user.
pub fn default_pipe_name() -> String {
    format!("confluence-{}", std::env::var("USERNAME").unwrap_or_else(|_| "user".into()))
}

/// Owned security descriptor granting full access to SYSTEM and the current user only.
struct UserOnlySecurity(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is heap memory owned exclusively by this value.
unsafe impl Send for UserOnlySecurity {}

/// A pipe handle moved to the accept thread, which becomes its only user.
struct OwnedPipe(HANDLE);

// SAFETY: kernel handles may be used from any thread; ownership is unique.
unsafe impl Send for OwnedPipe {}

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

impl PipeServer {
    /// Starts accepting connections on `\\.\pipe\<name>`. Fails if another
    /// process already owns that pipe name.
    pub fn start(name: &str, handler: Handler) -> io::Result<Self> {
        let path = pipe_path(name);
        let security = UserOnlySecurity::new()?;
        // Create the first instance here so a name clash is reported to the caller.
        let first = OwnedPipe(create_instance(&path, &security, true)?);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::Builder::new()
                .name("confluence-pipe".into())
                .spawn(move || accept_loop(&path, first, security, &stop, handler))?
        };
        Ok(Self { path, stop, thread: Some(thread) })
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
    let mut next = Some(first.0);
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
            Ok(Some(env)) => {
                let resp = handler(&env.body);
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

/// Blocking Control API client.
pub struct PipeClient {
    file: File,
    next_id: u32,
}

impl PipeClient {
    /// Connects, retrying for up to `timeout` while the pipe is missing or busy.
    pub fn connect(name: &str, timeout: Duration) -> io::Result<Self> {
        let path = pipe_path(name);
        let deadline = Instant::now() + timeout;
        loop {
            match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => return Ok(Self { file, next_id: 1 }),
                Err(e) if Instant::now() < deadline && matches!(e.raw_os_error(), Some(2) | Some(231)) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn call(&mut self, cmd: Command) -> Result<Response, FrameError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        write_frame(&mut self.file, &Envelope::new(id, cmd))?;
        match read_envelope::<_, Response>(&mut self.file)? {
            Some(env) => Ok(env.body),
            None => Err(FrameError::Io(io::Error::from(io::ErrorKind::UnexpectedEof))),
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
            Arc::new(move |cmd: &Command| {
                seen.lock().unwrap().push(cmd.clone());
                Response::Ok
            })
        };
        let name = unique_name("roundtrip");
        let server = PipeServer::start(&name, handler).unwrap();
        let mut a = PipeClient::connect(&name, Duration::from_secs(2)).unwrap();
        let mut b = PipeClient::connect(&name, Duration::from_secs(2)).unwrap();
        assert_eq!(a.call(Command::ListPoints).unwrap(), Response::Ok);
        assert_eq!(b.call(Command::Health).unwrap(), Response::Ok);
        assert_eq!(a.call(Command::ListSlots).unwrap(), Response::Ok);
        server.stop();
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn second_server_on_same_name_is_refused() {
        let name = unique_name("clash");
        let handler: Handler = Arc::new(|_: &Command| Response::Ok);
        let _server = PipeServer::start(&name, handler.clone()).unwrap();
        assert!(PipeServer::start(&name, handler).is_err());
    }

    #[test]
    fn hostile_client_is_dropped_and_others_keep_working() {
        use std::io::{Read, Write};
        let name = unique_name("hostile");
        let handler: Handler = Arc::new(|_: &Command| Response::Ok);
        let _server = PipeServer::start(&name, handler).unwrap();
        let mut raw = OpenOptions::new().read(true).write(true).open(pipe_path(&name)).unwrap();
        raw.write_all(&u32::MAX.to_le_bytes()).unwrap();
        let mut byte = [0u8; 1];
        assert!(!matches!(raw.read(&mut byte), Ok(1)), "server hung up instead of answering");
        let mut good = PipeClient::connect(&name, Duration::from_secs(2)).unwrap();
        assert_eq!(good.call(Command::Health).unwrap(), Response::Ok);
    }
}
