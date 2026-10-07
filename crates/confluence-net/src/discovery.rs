//! Finding other Confluence engines: DNS-SD service `_confluence._udp`
//! (Windows' own `DnsService*` API), behind a trait so tests use a fake.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use confluence_api::Peer;

/// The DNS-SD service type.
pub const SERVICE: &str = "_confluence._udp.local";
/// A peer not seen again for this long is dropped.
pub const PEER_TTL: Duration = Duration::from_secs(120);

/// The engine name in a service instance name (`Lilith._confluence._udp.local`).
pub fn engine_name(instance: &str) -> Option<String> {
    let full = instance.trim_end_matches('.');
    let suffix = format!(".{SERVICE}");
    if full.len() <= suffix.len() || !full.to_ascii_lowercase().ends_with(&suffix) {
        return None;
    }
    let name = &full[..full.len() - suffix.len()];
    (!name.is_empty()).then(|| name.to_string())
}

/// Peers seen, by engine id (a renamed engine replaces its old entry).
pub struct PeerTable {
    me: u64,
    by_id: HashMap<u64, (Peer, Instant)>,
}

impl PeerTable {
    pub fn new(me: u64) -> PeerTable {
        PeerTable { me, by_id: HashMap::new() }
    }

    pub fn seen(&mut self, name: &str, address: &str, port: u16, id: u64, now: Instant) {
        let peer = Peer { name: name.to_string(), address: address.to_string(), port };
        self.by_id.insert(id, (peer, now));
    }

    /// Other engines seen within [`PEER_TTL`], sorted by name.
    pub fn list(&mut self, now: Instant) -> Vec<Peer> {
        self.by_id.retain(|_, (_, at)| now.saturating_duration_since(*at) < PEER_TTL);
        let mut v: Vec<Peer> =
            self.by_id.iter().filter(|(id, _)| **id != self.me).map(|(_, (p, _))| p.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

/// Something that finds other engines.
pub trait Discovery: Send {
    fn peers(&self) -> Vec<Peer>;
}

/// Reports the peers it is given (tests, `--no-net-discovery`).
#[derive(Clone, Default)]
pub struct FakeDiscovery(pub Arc<Mutex<Vec<Peer>>>);

impl Discovery for FakeDiscovery {
    fn peers(&self) -> Vec<Peer> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(windows)]
pub use windows_dnssd::DnsSd;

#[cfg(windows)]
mod windows_dnssd {
    use super::*;
    use std::net::{Ipv4Addr, ToSocketAddrs};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::NetworkManagement::Dns::{
        DnsFree, DnsFreeRecordList, DnsServiceBrowse, DnsServiceBrowseCancel, DnsServiceConstructInstance,
        DnsServiceDeRegister, DnsServiceFreeInstance, DnsServiceRegister, DnsServiceResolve,
        DNS_QUERY_REQUEST_VERSION1, DNS_RECORDW, DNS_SERVICE_BROWSE_REQUEST, DNS_SERVICE_CANCEL, DNS_SERVICE_INSTANCE,
        DNS_SERVICE_REGISTER_REQUEST, DNS_SERVICE_RESOLVE_REQUEST, DNS_TYPE_PTR,
    };

    /// Browsing is restarted this often, so peers that are still there are seen again.
    const REBROWSE: Duration = Duration::from_secs(30);
    /// An instance is resolved again at most this often.
    const RESOLVE_EVERY: Duration = Duration::from_secs(10);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// # Safety
    /// `p` is null or a NUL-terminated UTF-16 string.
    unsafe fn text(p: PWSTR) -> Option<String> {
        if p.is_null() {
            None
        } else {
            unsafe { p.to_string().ok() }
        }
    }

    /// Shared with the callbacks; lives for the rest of the process (callbacks
    /// may still arrive after a cancel).
    struct Ctx {
        table: Mutex<PeerTable>,
        resolved: Mutex<HashMap<String, Instant>>,
        live: AtomicBool,
    }

    struct Resolve {
        ctx: &'static Ctx,
        name: Vec<u16>,
        request: DNS_SERVICE_RESOLVE_REQUEST,
        cancel: DNS_SERVICE_CANCEL,
    }

    unsafe extern "system" fn on_resolved(
        status: u32,
        context: *const core::ffi::c_void,
        inst: *const DNS_SERVICE_INSTANCE,
    ) {
        // SAFETY: the context is the Box leaked in `resolve`, given back once.
        let job = unsafe { Box::from_raw(context as *mut Resolve) };
        if status != 0 || inst.is_null() {
            return;
        }
        // SAFETY: Windows hands us a valid instance, ours to free.
        let i = unsafe { &*inst };
        let found = (|| {
            let name = engine_name(&unsafe { text(i.pszInstanceName) }?)?;
            let mut id = None;
            for k in 0..i.dwPropertyCount as usize {
                // SAFETY: `dwPropertyCount` keys and values.
                let (key, value) = unsafe { (text(*i.keys.add(k)), text(*i.values.add(k))) };
                if key.as_deref() == Some("id") {
                    id = value.and_then(|v| u64::from_str_radix(&v, 16).ok());
                }
            }
            let address = if i.ip4Address.is_null() {
                let host = unsafe { text(i.pszHostName) }?;
                (host.as_str(), i.wPort).to_socket_addrs().ok()?.find(|a| a.is_ipv4())?.ip().to_string()
            } else {
                // SAFETY: non-null IPv4 address, stored in network byte order.
                Ipv4Addr::from(unsafe { *i.ip4Address }.to_ne_bytes()).to_string()
            };
            Some((name, address, i.wPort, id?))
        })();
        // SAFETY: from the resolve completion; freed once.
        unsafe { DnsServiceFreeInstance(inst) };
        if let Some((name, address, port, id)) = found {
            if job.ctx.live.load(Ordering::Relaxed) {
                job.ctx.table.lock().unwrap_or_else(|p| p.into_inner()).seen(&name, &address, port, id, Instant::now());
            }
        }
    }

    fn resolve(ctx: &'static Ctx, instance: &str) {
        {
            let mut r = ctx.resolved.lock().unwrap_or_else(|p| p.into_inner());
            if r.get(instance).is_some_and(|t| t.elapsed() < RESOLVE_EVERY) {
                return;
            }
            r.insert(instance.to_string(), Instant::now());
        }
        let mut job = Box::new(Resolve {
            ctx,
            name: wide(instance),
            request: DNS_SERVICE_RESOLVE_REQUEST::default(),
            cancel: DNS_SERVICE_CANCEL::default(),
        });
        job.request.Version = DNS_QUERY_REQUEST_VERSION1.0;
        job.request.QueryName = PWSTR(job.name.as_mut_ptr());
        job.request.pResolveCompletionCallback = Some(on_resolved);
        let raw = Box::into_raw(job);
        // SAFETY: the job (request, name, cancel) stays alive until the callback frees it.
        unsafe {
            (*raw).request.pQueryContext = raw as *mut _;
            if DnsServiceResolve(&(*raw).request, &mut (*raw).cancel) != windows::Win32::Foundation::DNS_REQUEST_PENDING
            {
                drop(Box::from_raw(raw));
            }
        }
    }

    unsafe extern "system" fn on_browse(status: u32, context: *const core::ffi::c_void, records: *const DNS_RECORDW) {
        // SAFETY: the context is the process-lifetime Ctx.
        let ctx: &'static Ctx = unsafe { &*(context as *const Ctx) };
        let mut r = records;
        while status == 0 && !r.is_null() {
            // SAFETY: a record list from Windows.
            let rec = unsafe { &*r };
            if rec.wType == DNS_TYPE_PTR.0 && rec.dwTtl > 0 {
                // SAFETY: PTR records carry a host name.
                if let Some(instance) = unsafe { text(rec.Data.PTR.pNameHost) } {
                    if engine_name(&instance).is_some() && ctx.live.load(Ordering::Relaxed) {
                        resolve(ctx, &instance);
                    }
                }
            }
            r = rec.pNext;
        }
        if !records.is_null() {
            // SAFETY: the list given to this callback is ours to free.
            unsafe { DnsFree(Some(records as *const _), DnsFreeRecordList) };
        }
    }

    /// Registers this engine and browses for others until dropped.
    pub struct DnsSd {
        ctx: &'static Ctx,
        instance: *mut DNS_SERVICE_INSTANCE,
        register: Box<DNS_SERVICE_REGISTER_REQUEST>,
        stop: Arc<AtomicBool>,
        browser: Option<JoinHandle<()>>,
    }

    // SAFETY: the instance pointer is only used to deregister, from Drop.
    unsafe impl Send for DnsSd {}

    unsafe extern "system" fn on_registered(_: u32, _: *const core::ffi::c_void, inst: *const DNS_SERVICE_INSTANCE) {
        if !inst.is_null() {
            // SAFETY: the completion hands us a copy to free.
            unsafe { DnsServiceFreeInstance(inst) };
        }
    }

    impl DnsSd {
        /// Advertises `name` on `port` with engine id `id`, and starts looking for others.
        pub fn start(name: &str, port: u16, id: u64) -> Result<DnsSd, String> {
            let ctx: &'static Ctx = Box::leak(Box::new(Ctx {
                table: Mutex::new(PeerTable::new(id)),
                resolved: Mutex::new(HashMap::new()),
                live: AtomicBool::new(true),
            }));
            let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "confluence".into());
            let (service, host) = (wide(&format!("{name}.{SERVICE}")), wide(&format!("{host}.local")));
            let (k_id, k_v, v_id, v_v) = (wide("id"), wide("v"), wide(&format!("{id:x}")), wide("1"));
            let keys = [PCWSTR(k_id.as_ptr()), PCWSTR(k_v.as_ptr())];
            let values = [PCWSTR(v_id.as_ptr()), PCWSTR(v_v.as_ptr())];
            // SAFETY: valid NUL-terminated strings; the instance is freed in Drop.
            let instance = unsafe {
                DnsServiceConstructInstance(
                    PCWSTR(service.as_ptr()),
                    PCWSTR(host.as_ptr()),
                    None,
                    None,
                    port,
                    0,
                    0,
                    2,
                    keys.as_ptr(),
                    values.as_ptr(),
                )
            };
            if instance.is_null() {
                return Err("could not describe this engine for discovery".into());
            }
            let mut register = Box::new(DNS_SERVICE_REGISTER_REQUEST::default());
            register.Version = DNS_QUERY_REQUEST_VERSION1.0;
            register.pServiceInstance = instance;
            register.pRegisterCompletionCallback = Some(on_registered);
            // SAFETY: the request and instance live until Drop deregisters.
            let status = unsafe { DnsServiceRegister(&*register, None) };
            if status != windows::Win32::Foundation::DNS_REQUEST_PENDING as u32 {
                // SAFETY: constructed above, not registered.
                unsafe { DnsServiceFreeInstance(instance) };
                return Err(format!("discovery registration failed ({status})"));
            }
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let browser = std::thread::Builder::new()
                .name("confluence-dnssd".into())
                .spawn(move || browse_loop(ctx, &s))
                .map_err(|e| e.to_string())?;
            Ok(DnsSd { ctx, instance, register, stop, browser: Some(browser) })
        }
    }

    fn browse_loop(ctx: &'static Ctx, stop: &AtomicBool) {
        let query = wide(SERVICE);
        while !stop.load(Ordering::Relaxed) {
            let mut request = DNS_SERVICE_BROWSE_REQUEST {
                Version: DNS_QUERY_REQUEST_VERSION1.0,
                QueryName: PCWSTR(query.as_ptr()),
                pQueryContext: ctx as *const Ctx as *mut _,
                ..Default::default()
            };
            request.Anonymous.pBrowseCallback = Some(on_browse);
            let mut cancel = DNS_SERVICE_CANCEL::default();
            // SAFETY: request and cancel outlive the browse, which is cancelled below.
            let ok =
                unsafe { DnsServiceBrowse(&request, &mut cancel) } == windows::Win32::Foundation::DNS_REQUEST_PENDING;
            let until = Instant::now() + REBROWSE;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
            }
            if ok {
                // SAFETY: the handle from the browse above.
                unsafe { DnsServiceBrowseCancel(&cancel) };
            }
        }
    }

    impl Discovery for DnsSd {
        fn peers(&self) -> Vec<Peer> {
            self.ctx.table.lock().unwrap_or_else(|p| p.into_inner()).list(Instant::now())
        }
    }

    impl Drop for DnsSd {
        fn drop(&mut self) {
            self.ctx.live.store(false, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            if let Some(b) = self.browser.take() {
                let _ = b.join();
            }
            // SAFETY: registered in `start`; deregistering is synchronous enough
            // for the request; the instance is ours.
            unsafe {
                DnsServiceDeRegister(&*self.register, None);
                DnsServiceFreeInstance(self.instance);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn instance_names_give_the_engine_name() {
        assert_eq!(engine_name("Lilith._confluence._udp.local"), Some("Lilith".into()));
        assert_eq!(engine_name("Studio PC._confluence._udp.local."), Some("Studio PC".into()));
        assert_eq!(engine_name("Printer._ipp._tcp.local"), None);
        assert_eq!(engine_name("._confluence._udp.local"), None);
    }

    #[test]
    fn the_table_lists_others_sorted_and_forgets_the_silent() {
        let t0 = Instant::now();
        let mut t = PeerTable::new(42);
        t.seen("Zed", "192.168.50.20", 6990, 7, t0);
        t.seen("Lilith", "192.168.50.12", 6990, 9, t0);
        t.seen("Me", "192.168.50.11", 6990, 42, t0);
        let names: Vec<_> = t.list(t0).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Lilith", "Zed"], "sorted, without this engine");
        t.seen("Lilith", "192.168.50.13", 6991, 9, t0 + Duration::from_secs(100));
        let later = t.list(t0 + PEER_TTL + Duration::from_secs(1));
        assert_eq!(later.len(), 1, "Zed went quiet");
        assert_eq!((later[0].address.as_str(), later[0].port), ("192.168.50.13", 6991), "Lilith moved");
    }

    #[test]
    fn a_renamed_engine_replaces_its_old_name() {
        let t0 = Instant::now();
        let mut t = PeerTable::new(1);
        t.seen("Old", "10.0.0.2", 6990, 5, t0);
        t.seen("New", "10.0.0.2", 6990, 5, t0);
        let names: Vec<_> = t.list(t0).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["New"]);
    }

    #[test]
    fn the_fake_reports_what_it_is_given() {
        let fake = FakeDiscovery::default();
        assert!(fake.peers().is_empty());
        fake.0.lock().unwrap().push(Peer { name: "Lilith".into(), address: "127.0.0.1".into(), port: 16990 });
        assert_eq!(fake.peers().len(), 1);
    }

    /// Registers on the real network: run by hand (`--ignored`), never in CI.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn this_engine_is_found_by_another_browser_on_the_network() {
        let me = DnsSd::start("confluence-selftest", 16_990, 1).expect("register");
        let other = DnsSd::start("confluence-selftest-2", 16_991, 2).expect("register");
        let until = Instant::now() + Duration::from_secs(20);
        while !other.peers().iter().any(|p| p.name == "confluence-selftest") {
            assert!(Instant::now() < until, "not found: {:?}", other.peers());
            std::thread::sleep(Duration::from_millis(200));
        }
        drop(me);
    }
}
