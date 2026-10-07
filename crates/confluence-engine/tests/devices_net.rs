//! Network streams as device bindings, in-process on loopback: what a stream
//! added by an engine's name needs to come back after a restart.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use confluence_api::{Command, DeviceKind, Peer, Response};
use confluence_core::buffer::PlanarBuffer;
use confluence_engine::clock::InternalClock;
use confluence_engine::devices::{AsioOpener, DeviceManager, NetCtx};
use confluence_engine::{Engine, EngineConfig};
use confluence_net::discovery::FakeDiscovery;
use confluence_net::host::{NetHost, SendSpec};

fn loopback(port: u16) -> SocketAddr {
    (Ipv4Addr::LOCALHOST, port).into()
}

/// Names are never looked up on the real network in tests.
fn net(host: NetHost, peers: &Arc<Mutex<Vec<Peer>>>) -> NetCtx {
    confluence_provider_vasio::isolate_for_tests();
    NetCtx::new(host, Box::new(FakeDiscovery(peers.clone()))).with_lookup(Arc::new(|_: &str, _: u16| None))
}

/// Another engine sending a 4-channel 44.1 kHz stream "Main" to `dest` until dropped.
struct Sender {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    port: u16,
}

impl Sender {
    fn start(dest: SocketAddr) -> Sender {
        let host = NetHost::start(loopback(0), 7).unwrap();
        let port = host.port();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || {
            let (mut side, _h) = host.add_sender(SendSpec { dest, stream: "Main".into(), channels: 4, rate: 44_100 });
            let mut block = PlanarBuffer::new(4, 441);
            for c in 0..4 {
                block.channel_mut(c).fill(0.25);
            }
            while !s.load(Ordering::Relaxed) {
                side.write(&block, 0);
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(host);
        });
        Sender { stop, thread: Some(thread), port }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_stream_added_by_engine_name_comes_back_with_its_channels_and_rate_once_the_engine_is_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices.json");
    let host = NetHost::start(loopback(0), 1).unwrap();
    let port = host.port();
    let sender = Sender::start(loopback(port));
    let peers =
        Arc::new(Mutex::new(vec![Peer { name: "Other".into(), address: "127.0.0.1".into(), port: sender.port }]));
    {
        let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
        let (devices, _) = DeviceManager::open_file(path.clone());
        let mut devices = devices.with_net(net(host, &peers));
        wait_until("the stream to be heard", || {
            devices.net_devices().iter().any(|d| d.name == "Other/Main" && d.inputs == 4)
        });
        devices.add(&mut engine, DeviceKind::NetReceive, "Other/Main").unwrap();
        assert_eq!(engine.slots()[0].inputs, 4);
    }
    // Restart before discovery has found the other engine again.
    peers.lock().unwrap().clear();
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let (devices, _) = DeviceManager::open_file(path.clone());
    let mut devices = devices.with_net(net(NetHost::start(loopback(port), 1).unwrap(), &peers));
    let warnings = devices.restore(&mut engine);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let net_slot = |e: &Engine| e.slots().into_iter().find(|s| s.device.starts_with("net-in:")).unwrap();
    assert!(!net_slot(&engine).online);
    assert!(!devices.retry_offline_net(&mut engine), "still not found");
    // Found: it comes back by itself, as it was.
    peers.lock().unwrap().push(Peer { name: "Other".into(), address: "127.0.0.1".into(), port: sender.port });
    assert!(devices.retry_offline_net(&mut engine));
    let slot = net_slot(&engine);
    assert!(slot.online, "{slot:?}");
    assert_eq!((slot.first_input, slot.inputs), (0, 4));
    let binding = devices.bindings().into_iter().find(|b| b.kind == DeviceKind::NetReceive).unwrap();
    assert_eq!(binding.rate, Some(44_100));
    assert!(!devices.retry_offline_net(&mut engine), "nothing left to retry");
}

#[test]
fn an_engine_name_not_found_by_discovery_is_looked_up_by_name() {
    let peers = Arc::new(Mutex::new(Vec::new()));
    let (mut engine, _audio) = Engine::new(EngineConfig::new(48_000.0, 256));
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let seen = lookups.clone();
    let ctx = NetCtx::new(NetHost::start(loopback(0), 1).unwrap(), Box::new(FakeDiscovery(peers))).with_lookup(
        Arc::new(move |name: &str, port: u16| {
            seen.lock().unwrap().push(name.to_string());
            Some(loopback(port))
        }),
    );
    let mut devices = DeviceManager::new(None).with_net(ctx);
    devices.add(&mut engine, DeviceKind::NetSend, "studio-pc/Main").unwrap();
    assert_eq!(*lookups.lock().unwrap(), ["studio-pc"]);
    assert_eq!(engine.slots()[0].outputs, 2);
}

/// Opens `fake:<name>` as a fake ASIO driver (2 in / 2 out, every input 0.25)
/// whose probe is shared with the test.
fn fake_asio(probes: Vec<(&'static str, Arc<confluence_provider_asio::fake::FakeProbe>)>) -> AsioOpener {
    use confluence_provider_asio::fake::FakeConfig;
    use confluence_provider_asio::{AsioDevice, AsioHostError, DriverSource};
    Box::new(move |name: &str| {
        let (_, probe) = probes.iter().find(|(n, _)| *n == name).ok_or(AsioHostError::NotInstalled(name.into()))?;
        let mut cfg = FakeConfig::new(name);
        cfg.probe = probe.clone();
        AsioDevice::open(DriverSource::Fake(cfg))
    })
}

fn route(engine: &mut Engine, input: u32, output: u32, on: bool) {
    let cmd = if on {
        Command::SetPoint { input, output, gain_db: 0.0, mute: false, invert: false }
    } else {
        Command::RemovePoint { input, output }
    };
    assert_eq!(engine.handle(&cmd), Response::Ok);
}

/// Runs both engines' control ticks for `ms`.
fn run(engines: &mut [&mut Engine], ms: u64) {
    for _ in 0..ms / 10 {
        std::thread::sleep(Duration::from_millis(10));
        for e in engines.iter_mut() {
            e.tick();
        }
    }
}

#[test]
fn audio_routed_to_a_send_stream_plays_from_the_other_engines_receive_stream() {
    use confluence_provider_asio::fake::FakeProbe;
    let (pa, pb) = (Arc::new(FakeProbe::default()), Arc::new(FakeProbe::default()));
    let peers = Arc::new(Mutex::new(Vec::new()));
    // A: a device's input 1 (0.25) goes to channel 2 only of a send stream to B.
    let (mut a, audio_a) = Engine::new(EngineConfig::new(48_000.0, 256));
    let host_b = NetHost::start(loopback(0), 2).unwrap();
    let port_b = host_b.port();
    let mut da = DeviceManager::new(None)
        .with_asio_opener(fake_asio(vec![("fake:a", pa)]))
        .with_net(net(NetHost::start(loopback(0), 1).unwrap(), &peers));
    da.add(&mut a, DeviceKind::Asio, "fake:a").unwrap();
    da.add(&mut a, DeviceKind::NetSend, &format!("127.0.0.1:{port_b}/Main:2")).unwrap();
    let slots = a.slots();
    let dev_in = slots.iter().find(|s| s.name == "fake:a in").unwrap().first_input;
    let send = slots.iter().find(|s| s.device.starts_with("net-out:")).unwrap().first_output;
    route(&mut a, dev_in, send + 1, true);
    let clock_a = InternalClock::start(audio_a, 48_000.0).unwrap();

    // B: the stream's channels play, one at a time, on a device's output 1.
    let (mut b, audio_b) = Engine::new(EngineConfig::new(48_000.0, 256));
    let mut db = DeviceManager::new(None)
        .with_asio_opener(fake_asio(vec![("fake:b", pb.clone())]))
        .with_net(net(host_b, &peers));
    db.add(&mut b, DeviceKind::Asio, "fake:b").unwrap();
    db.add(&mut b, DeviceKind::NetReceive, "127.0.0.1/Main").unwrap();
    let slots = b.slots();
    let recv = slots.iter().find(|s| s.device.starts_with("net-in:")).unwrap().first_input;
    let dev_out = slots.iter().find(|s| s.name == "fake:b out").unwrap().first_output;
    route(&mut b, recv + 1, dev_out, true);
    let clock_b = InternalClock::start(audio_b, 48_000.0).unwrap();

    run(&mut [&mut a, &mut b], 1500); // the receive stream settles
    for _ in 0..20 {
        run(&mut [&mut a, &mut b], 50);
        let last = pb.last_output.lock().unwrap().clone();
        assert!(last.iter().all(|&s| (s - 0.25).abs() < 2e-3), "channel 2 arrives steadily: {:?}", &last[..4]);
    }
    // Channel 1 carries nothing.
    route(&mut b, recv + 1, dev_out, false);
    route(&mut b, recv, dev_out, true);
    run(&mut [&mut a, &mut b], 300);
    let last = pb.last_output.lock().unwrap().clone();
    assert!(last.iter().all(|&s| s.abs() < 1e-3), "channel 1 is silent: {:?}", &last[..4]);
    clock_a.stop();
    clock_b.stop();
}
