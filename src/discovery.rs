// Finding projectors on the LAN without any typing:
//  * every receiver broadcasts a HELLO to UDP 47801 once a second (we listen there),
//  * we additionally broadcast a PROBE to port 47800 so a freshly started receiver answers at once,
//  * and we browse mDNS (_skreenshare._udp) — a hit just triggers a unicast PROBE, so all roads lead to HELLO.
// Manual "add by IP" probes the same way.
use proto::{self as p, Hello};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Device {
    pub ip: Ipv4Addr,
    pub hello: Hello,
    pub last_seen: Instant,
}

impl Device {
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(self.ip, self.hello.port))
    }
}

#[derive(Default)]
struct State {
    devices: HashMap<Ipv4Addr, Device>,
    /// Manually added addresses: probed forever until they answer.
    manual: Vec<Ipv4Addr>,
}

pub struct Discovery {
    state: Arc<Mutex<State>>,
    running: Arc<AtomicBool>,
}

const STALE: Duration = Duration::from_secs(5);

impl Discovery {
    pub fn start() -> Discovery {
        let state = Arc::new(Mutex::new(State::default()));
        let running = Arc::new(AtomicBool::new(true));

        // Listener on 47801 (+ a probe sender sharing the same socket so replies come back here).
        {
            let state = state.clone();
            let running = running.clone();
            std::thread::spawn(move || listen(state, running));
        }
        // mDNS browser (best effort).
        {
            let state = state.clone();
            let running = running.clone();
            std::thread::spawn(move || browse_mdns(state, running));
        }
        Discovery { state, running }
    }

    /// Devices heard from recently, sorted by name.
    pub fn devices(&self) -> Vec<Device> {
        let mut v: Vec<Device> = self
            .state
            .lock()
            .unwrap()
            .devices
            .values()
            .filter(|d| d.last_seen.elapsed() < STALE)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.hello.name.cmp(&b.hello.name).then(a.ip.cmp(&b.ip)));
        v
    }

    pub fn add_manual(&self, ip: Ipv4Addr) {
        let mut s = self.state.lock().unwrap();
        if !s.manual.contains(&ip) {
            s.manual.push(ip);
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn bind_discovery_socket() -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_broadcast(true)?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, p::DISCOVERY_PORT)).into())?;
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(s)
}

fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut v = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if let if_addrs::IfAddr::V4(a) = i.addr {
                if !a.ip.is_loopback() && !a.ip.is_link_local() {
                    let b = a.broadcast.unwrap_or_else(|| {
                        let ip = u32::from(a.ip);
                        let mask = u32::from(a.netmask);
                        Ipv4Addr::from(ip | !mask)
                    });
                    if !v.contains(&b) {
                        v.push(b);
                    }
                }
            }
        }
    }
    v
}

fn listen(state: Arc<Mutex<State>>, running: Arc<AtomicBool>) {
    let mut sock = None;
    let mut buf = [0u8; 1500];
    let mut probe = Vec::new();
    p::write_probe(p::DISCOVERY_PORT, &mut probe);
    let mut last_probe = Instant::now() - Duration::from_secs(10);
    let mut targets;
    while running.load(Ordering::Relaxed) {
        if sock.is_none() {
            sock = bind_discovery_socket().ok();
            if sock.is_none() {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }
        let s = sock.as_ref().unwrap();
        if last_probe.elapsed() >= Duration::from_secs(2) {
            last_probe = Instant::now();
            targets = broadcast_targets(); // networks come and go (VPN, Wi-Fi switch)
            for t in &targets {
                let _ = s.send_to(&probe, SocketAddrV4::new(*t, p::PORT));
            }
            let manual = state.lock().unwrap().manual.clone();
            for ip in manual {
                let _ = s.send_to(&probe, SocketAddrV4::new(ip, p::PORT));
            }
        }
        match s.recv_from(&mut buf) {
            Ok((n, SocketAddr::V4(from))) => {
                if let Some((p::T_HELLO, _, body)) = p::parse_header(&buf[..n]) {
                    if let Some(h) = p::parse_hello(body) {
                        let ip = *from.ip();
                        state.lock().unwrap().devices.insert(ip, Device { ip, hello: h, last_seen: Instant::now() });
                    }
                }
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(100)), // e.g. ICMP "port unreachable" resets on Windows
        }
    }
}

fn browse_mdns(state: Arc<Mutex<State>>, running: Arc<AtomicBool>) {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let Ok(daemon) = ServiceDaemon::new() else { return };
    let Ok(rx) = daemon.browse(p::MDNS_TYPE) else { return };
    let _ = state; // results go through the normal HELLO path
    let probe_sock = UdpSocket::bind("0.0.0.0:0").ok();
    let mut probe = Vec::new();
    p::write_probe(p::DISCOVERY_PORT, &mut probe);
    while running.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(s) = &probe_sock {
                    let port = info.get_port();
                    for ip in info.get_addresses_v4() {
                        let _ = s.send_to(&probe, SocketAddrV4::new(*ip, port));
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    let _ = daemon.shutdown();
}
