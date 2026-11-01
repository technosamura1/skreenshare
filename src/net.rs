// UDP transport: packet format, STUN discovery, connection codes and NAT hole punching.
use anyhow::{bail, Context, Result};
use base64::Engine;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Max video payload per datagram; keeps packets well under common MTUs (PPPoE, tunnels).
pub const MAX_PAYLOAD: usize = 1200;

// Packet types. All are >= 0x80 so they never collide with STUN (first byte 0x00/0x01).
pub const PT_PUNCH: u8 = 0x81;
pub const PT_PUNCH_ACK: u8 = 0x82;
pub const PT_VIDEO: u8 = 0x90;
pub const PT_AUDIO: u8 = 0x91; // legacy (v0.5.0 and older): seq u32 | rate u32 | samples
/// Audio with loss protection, see audiopkt.rs.
pub const PT_AUDIO2: u8 = 0x92;
/// Opus audio (v0.7.4+), see opusaudio.rs. Only sent to viewers that announced it with PT_CAPS.
pub const PT_AUDIO3: u8 = 0x93;
pub const PT_KEYREQ: u8 = 0xA0;
pub const PT_PING: u8 = 0xA1;
pub const PT_PONG: u8 = 0xA2;
pub const PT_BYE: u8 = 0xA3;
/// Client -> host: resend these fragments. frame_id u32 | n u16 | n × frag_idx u16 (n = 0: whole frame).
pub const PT_NACK: u8 = 0xA4;
/// Client -> host: running totals (u64 each): fragments received | found missing | still missing after a
/// repeat request | frames given up on. Older clients send only the first two.
pub const PT_REPORT: u8 = 0xA5;
/// Client -> host: resend these audio datagrams. n u8 | n × seq u32.
pub const PT_ANACK: u8 = 0xA6;
/// Client -> host: what this viewer understands (sent about once a second): flags u8, bit 0 = Opus audio.
pub const PT_CAPS: u8 = 0xA7;
pub const CAP_OPUS: u8 = 1;
/// The viewer understands PT_CTRL / PT_RUMBLE and may send PT_INPUT (remote control, v0.11.0).
pub const CAP_INPUT: u8 = 2;
/// Remote control (see inputproto.rs): viewer -> host input state, host -> viewer permission and driver state,
/// host -> viewer force feedback. Old programs ignore these types.
pub const PT_INPUT: u8 = 0xA8;
pub const PT_CTRL: u8 = 0xA9;
pub const PT_RUMBLE: u8 = 0xAA;
/// Viewer -> host laser pointer and reactions (annot.rs, v0.11.8). Old hosts ignore it.
pub const PT_ANNOT: u8 = 0xAB;
/// Viewer -> host: «I'd like to control» (ver u8 = 1 | want u8), about once a second while waiting (v0.12.8).
pub const PT_CTLREQ: u8 = 0xAC;
/// Encrypted video (v0.12.11). Clear header is the same as PT_VIDEO, then nonce and ciphertext.
pub const PT_VIDEO_ENC: u8 = 0x94;
/// Encrypted Opus audio. Nonce carries the datagram sequence, so two packets never share it.
pub const PT_AUDIO_ENC: u8 = 0x95;
/// Encrypted legacy audio (PT_AUDIO2). Same idea as PT_AUDIO_ENC, separate stream number.
pub const PT_AUDIO2_ENC: u8 = 0x96;
/// Encrypted remote control. A network observer who can see the session token still cannot forge or read these.
pub const PT_INPUT_ENC: u8 = 0xB0;
pub const PT_ANNOT_ENC: u8 = 0xB1;
pub const PT_CTLREQ_ENC: u8 = 0xB2;
pub const PT_CTRL_ENC: u8 = 0xB3;
pub const PT_RUMBLE_ENC: u8 = 0xB4;
/// Authenticated signalling, including handshake, disconnect and feedback (0.13+).
pub const PT_SIGNAL_ENC: u8 = 0xB5;
/// Viewer understands encrypted media and has the friend secret. Host stays in the clear until it sees this.
pub const CAP_ENC: u8 = 4;

pub const ROLE_HOST: u8 = 1;
pub const ROLE_CLIENT: u8 = 2;

pub const HEADER_LEN: usize = 5;
/// type(1) token(4) | frame_id(4) frag_idx(2) frag_count(2) flags(1)
pub const VIDEO_HEADER_LEN: usize = HEADER_LEN + 9;

const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun.sipnet.ru:3478",
    "stun.nextcloud.com:3478",
];
const STUN_COOKIE: u32 = 0x2112_A442;

/// Test hook: SS_SIM_LOSS=0.02 makes the host drop 2% of the media packets it sends (off by default), to
/// rehearse a bad connection. Optional SS_SIM_BURST=4 makes the losses come in bursts of ~4 packets on average
/// (same overall rate), like Wi-Fi or an overfull router queue.
fn sim_cfg() -> (f64, f64) {
    static P: std::sync::OnceLock<(f64, f64)> = std::sync::OnceLock::new();
    *P.get_or_init(|| {
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().replace(',', ".").parse::<f64>().ok());
        let loss = get("SS_SIM_LOSS").unwrap_or(0.0).clamp(0.0, 0.95);
        (loss, get("SS_SIM_BURST").unwrap_or(1.0).max(1.0))
    })
}

fn rand01() -> f64 {
    u32::from_le_bytes(random_bytes::<4>()) as f64 / (u32::MAX as f64 + 1.0)
}

/// Whether the loss simulation eats this packet. A two-state chain: "bad" drops everything.
fn sim_drop() -> bool {
    static BAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let (p, burst) = sim_cfg();
    if p <= 0.0 {
        return false;
    }
    if burst <= 1.0 {
        return rand01() < p;
    }
    use std::sync::atomic::Ordering::Relaxed;
    let leave = 1.0 / burst; // mean bad-run length = burst
    let enter = p * leave / (1.0 - p); // stationary share of "bad" = p
    let bad = BAD.load(Relaxed);
    let next = if bad { rand01() >= leave } else { rand01() < enter };
    BAD.store(next, Relaxed);
    next
}

/// Video, audio and remote-control packets are encrypted once a key is installed.
/// Signalling and handshake are authenticated as well; encryption failure drops the packet.
pub fn send_media(sock: &UdpSocket, pkt: &[u8], to: SocketAddr) {
    if sim_drop() {
        return;
    }
    if pkt.len() >= HEADER_LEN {
        let token = u32::from_le_bytes(pkt[1..5].try_into().unwrap());
        absorb_media_pair(token);
    }
    if let Some(key) = armed_media_key() {
        if let Some(sealed) = seal_media(&key, pkt) {
            let _ = sock.send_to(&sealed, to);
        }
        return; // Encryption failure must never fall back to plaintext.
    }
    let _ = sock.send_to(pkt, to);
}

/// What a receiver should do with one datagram.
pub enum Incoming {
    /// Not an encrypted type: parse the buffer you already have.
    Plain,
    /// Encrypted type, opened back into the plaintext packet (PT_VIDEO / PT_AUDIO2 / PT_AUDIO3).
    Opened(Vec<u8>),
    /// Encrypted type we could not open (no key yet, or the tag failed).
    Drop,
}

pub fn open_incoming(pkt: &[u8]) -> Incoming {
    let ty = pkt.first().copied();
    let encrypted = matches!(
        ty,
        Some(PT_SIGNAL_ENC)
            | Some(PT_VIDEO_ENC)
            | Some(PT_AUDIO_ENC)
            | Some(PT_AUDIO2_ENC)
            | Some(PT_INPUT_ENC)
            | Some(PT_ANNOT_ENC)
            | Some(PT_CTLREQ_ENC)
            | Some(PT_CTRL_ENC)
            | Some(PT_RUMBLE_ENC)
    );
    if encrypted {
        for key in decrypt_keys() {
            if let Some(p) = open_media(&key, pkt) {
                if matches!(ty, Some(PT_SIGNAL_ENC | PT_INPUT_ENC | PT_ANNOT_ENC | PT_CTLREQ_ENC | PT_CTRL_ENC | PT_RUMBLE_ENC)) {
                    let nonce = &pkt[HEADER_LEN..HEADER_LEN + 12];
                    let role = nonce[10];
                    let mut g = match media_arm().lock() { Ok(g) => g, Err(_) => return Incoming::Drop };
                    if !matches!(role, ROLE_HOST | ROLE_CLIENT) || role == g.role { return Incoming::Drop; }
                    let seq = u64::from_le_bytes(nonce[..8].try_into().unwrap());
                    if !g.replays.entry((key, ty.unwrap())).or_default().accept(seq) { return Incoming::Drop; }
                }
                return Incoming::Opened(p);
            }
        }
        return Incoming::Drop;
    }
    // A v2 session has a key the packet headers do not carry. Clear picture or input would let anyone
    // who can see the token (it is in every header) inject keystrokes or frames.
    if session_armed() { return Incoming::Drop; }
    Incoming::Plain
}

fn seal_media(key: &[u8; 32], pkt: &[u8]) -> Option<Vec<u8>> {
    match pkt.first().copied()? {
        PT_VIDEO if pkt.len() >= VIDEO_HEADER_LEN => {
            let frame_id = u32::from_le_bytes(pkt[5..9].try_into().ok()?);
            let frag = u16::from_le_bytes(pkt[9..11].try_into().ok()?);
            crate::crypto::seal(key, pkt, PT_VIDEO_ENC, VIDEO_HEADER_LEN, frame_id, frag, 0)
        }
        PT_AUDIO3 if pkt.len() >= HEADER_LEN + 4 => {
            let seq = u32::from_le_bytes(pkt[5..9].try_into().ok()?);
            crate::crypto::seal(key, pkt, PT_AUDIO_ENC, HEADER_LEN, seq, 0, 1)
        }
        PT_AUDIO2 if pkt.len() >= HEADER_LEN + 4 => {
            let seq = u32::from_le_bytes(pkt[5..9].try_into().ok()?);
            crate::crypto::seal(key, pkt, PT_AUDIO2_ENC, HEADER_LEN, seq, 0, 2)
        }
        PT_INPUT => seal_ctrl(key, pkt, PT_INPUT_ENC, 3),
        PT_ANNOT => seal_ctrl(key, pkt, PT_ANNOT_ENC, 4),
        PT_CTLREQ => seal_ctrl(key, pkt, PT_CTLREQ_ENC, 5),
        PT_CTRL => seal_ctrl(key, pkt, PT_CTRL_ENC, 6),
        PT_RUMBLE => seal_ctrl(key, pkt, PT_RUMBLE_ENC, 7),
        _ if pkt.len() >= HEADER_LEN => {
            // Inner type and token are encrypted too. A clear outer token is only routing.
            let mut inner = pkt[..HEADER_LEN].to_vec();
            inner.extend_from_slice(pkt);
            seal_ctrl(key, &inner, PT_SIGNAL_ENC, 8)
        }
        _ => None,
    }
}

/// Control packets repeat and their own sequence is only 16 bits, which would reuse a ChaCha nonce.
/// The nonce here is a counter plus the stream, and it travels with the packet.
fn seal_ctrl(key: &[u8; 32], pkt: &[u8], enc_type: u8, stream: u16) -> Option<Vec<u8>> {
    if pkt.len() < HEADER_LEN {
        return None;
    }
    let (n, role) = {
        let mut g = media_arm().lock().ok()?;
        let n = g.ctrl_nonce;
        g.ctrl_nonce = g.ctrl_nonce.checked_add(1)?;
        (n, g.role)
    };
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&n.to_le_bytes());
    nonce[8..10].copy_from_slice(&stream.to_le_bytes());
    nonce[10] = role; // opposite directions must never share a nonce
    let mut header = pkt[..HEADER_LEN].to_vec();
    header[0] = enc_type;
    let ct = crate::crypto::encrypt_payload(key, &nonce, &header, &pkt[HEADER_LEN..])?;
    header.extend_from_slice(&nonce);
    header.extend_from_slice(&ct);
    Some(header)
}

fn open_media(key: &[u8; 32], pkt: &[u8]) -> Option<Vec<u8>> {
    match pkt.first().copied()? {
        PT_SIGNAL_ENC => {
            let opened = crate::crypto::open(key, pkt, PT_SIGNAL_ENC, HEADER_LEN)?;
            let inner = opened.get(HEADER_LEN..)?;
            if inner.len() < HEADER_LEN || inner[1..5] != pkt[1..5] { return None; }
            Some(inner.to_vec())
        }
        PT_VIDEO_ENC => crate::crypto::open(key, pkt, PT_VIDEO, VIDEO_HEADER_LEN),
        PT_AUDIO_ENC => crate::crypto::open(key, pkt, PT_AUDIO3, HEADER_LEN),
        PT_AUDIO2_ENC => crate::crypto::open(key, pkt, PT_AUDIO2, HEADER_LEN),
        PT_INPUT_ENC => crate::crypto::open(key, pkt, PT_INPUT, HEADER_LEN),
        PT_ANNOT_ENC => crate::crypto::open(key, pkt, PT_ANNOT, HEADER_LEN),
        PT_CTLREQ_ENC => crate::crypto::open(key, pkt, PT_CTLREQ, HEADER_LEN),
        PT_CTRL_ENC => crate::crypto::open(key, pkt, PT_CTRL, HEADER_LEN),
        PT_RUMBLE_ENC => crate::crypto::open(key, pkt, PT_RUMBLE, HEADER_LEN),
        _ => None,
    }
}

#[derive(Default)]
struct MediaArm {
    /// Derived key from the H… code. None for an old code: the stream stays clear.
    session: Option<[u8; 32]>,
    /// Derived friend-pair key.
    pair: Option<[u8; 32]>,
    /// Send with the pair key. Until the viewer confirms, the host keeps sending with the session key,
    /// which the viewer can also open. Nothing goes out in the clear once a session key exists.
    send_pair: bool,
    peer_enc: bool,
    ctrl_nonce: u64,
    role: u8,
    replays: std::collections::HashMap<([u8; 32], u8), crate::crypto::ReplayWindow>,
}

fn media_arm() -> &'static std::sync::Mutex<MediaArm> {
    use std::sync::OnceLock;
    static M: OnceLock<std::sync::Mutex<MediaArm>> = OnceLock::new();
    M.get_or_init(|| {
        std::sync::Mutex::new(MediaArm::default())
    })
}

fn armed_media_key() -> Option<[u8; 32]> {
    let g = media_arm().lock().ok()?;
    if g.send_pair { g.pair.or(g.session) } else { g.session }
}

fn decrypt_keys() -> Vec<[u8; 32]> {
    let Ok(g) = media_arm().lock() else { return Vec::new() };
    [g.pair, g.session].into_iter().flatten().collect()
}

fn session_armed() -> bool {
    media_arm().lock().ok().and_then(|g| g.session).is_some()
}

/// Viewer side, friend call: the pair key is known before the first packet.
/// The session key, if the code had one, stays installed so the host's first packets still open.
pub fn install_media_key(pair: &[u8; 32], _token: u32) {
    if let Ok(mut g) = media_arm().lock() {
        g.pair = g.session.map(|session| crate::crypto::friend_session_key(pair, &session));
        g.send_pair = true;
    }
}

/// Both sides of a v2 connection. The secret travelled only inside the H… code, never in a UDP header.
pub fn install_session_key(secret: &[u8; 32], token: u32, role: u8) {
    if let Ok(mut g) = media_arm().lock() {
        g.session = Some(crate::crypto::media_key(secret, token));
        g.role = role;
    }
}

/// Host side, from the GUI. Decrypt with the pair key immediately; keep sending with the session key
/// until the viewer says it has the pair key too ([`note_peer_enc`]).
pub fn note_media_pair(pair: [u8; 32], _token: u32) {
    if let Ok(mut g) = media_arm().lock() {
        g.pair = g.session.map(|session| crate::crypto::friend_session_key(&pair, &session));
        if g.peer_enc {
            g.send_pair = true;
        }
    }
}

/// `MEDIAKEY` may arrive on the GUI stdin while the host is still waiting for a code. Pick it up at the
/// next send or receive so it is not stuck until some unrelated packet.
pub fn absorb_media_pair(token: u32) {
    if let Some(pair) = crate::ui::take_media_pair() {
        note_media_pair(pair, token);
    }
}

/// Host side: the connected viewer sent [`CAP_ENC`].
pub fn note_peer_enc(_token: u32) {
    if let Ok(mut g) = media_arm().lock() {
        g.peer_enc = true;
        if g.pair.is_some() {
            g.send_pair = true;
        }
    }
}

pub fn write_header(buf: &mut Vec<u8>, ty: u8, token: u32) {
    buf.clear();
    buf.push(ty);
    buf.extend_from_slice(&token.to_le_bytes());
}

/// Returns (type, body) if the packet is ours and carries the session token.
pub fn parse_header(pkt: &[u8], token: u32) -> Option<(u8, &[u8])> {
    if pkt.len() < HEADER_LEN || pkt[0] < 0x80 {
        return None;
    }
    let t = u32::from_le_bytes(pkt[1..5].try_into().unwrap());
    (t == token).then(|| (pkt[0], &pkt[HEADER_LEN..]))
}

/// Errors from recv_from that just mean "nothing useful arrived" on Windows UDP sockets.
pub fn is_benign(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    )
}

/// Socket buffer sizes Windows actually granted (recv, send), in bytes: the requested sizes are only a wish.
pub fn socket_buffers(s: &UdpSocket) -> (usize, usize) {
    let r = socket2::SockRef::from(s);
    (r.recv_buffer_size().unwrap_or(0), r.send_buffer_size().unwrap_or(0))
}

pub fn bind_socket() -> Result<UdpSocket> {
    bind_socket_on(None)
}

/// `nic`: send and receive only through this adapter (see `open_and_discover`).
fn bind_socket_on(nic: Option<&Nic>) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Big buffers matter: a keyframe is hundreds of datagrams arriving back to back.
    let _ = s.set_recv_buffer_size(16 << 20);
    let _ = s.set_send_buffer_size(8 << 20);
    let ip = nic.map_or(Ipv4Addr::UNSPECIFIED, |n| n.ip);
    s.bind(&SocketAddr::from((ip, 0)).into())?;
    #[cfg(not(lossim))]
    if let Some(n) = nic {
        // Bound to the adapter's address *and* told to send through it: a VPN's catch-all route can't take it.
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IP_UNICAST_IF, SOCKET};
        let idx = n.index.to_be_bytes(); // IPv4 wants the index in network byte order
        unsafe { setsockopt(SOCKET(s.as_raw_socket() as usize), IPPROTO_IP.0, IP_UNICAST_IF, Some(&idx)) };
    }
    Ok(s.into())
}

/// A real network adapter with a way to the internet (Ethernet / Wi-Fi with a gateway).
pub struct Nic {
    pub ip: Ipv4Addr,
    pub index: u32,
    pub name: String,
}

/// The loss simulator (tools/lossim, any OS) has no Windows API: no adapter choice there.
#[cfg(lossim)]
pub fn physical_nic() -> Option<Nic> {
    None
}

/// The adapter our traffic would use without a VPN. VPN tunnels (Happ/sing-box, WireGuard, OpenVPN TAP,
/// Wintun...) are not Ethernet/Wi-Fi typed or are named as such, so they are skipped. Lowest metric wins.
#[cfg(not(lossim))]
pub fn physical_nic() -> Option<Nic> {
    use windows::Win32::NetworkManagement::IpHelper::*;
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    const ETHERNET: u32 = 6;
    const WIFI: u32 = 71;
    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 32 * 1024u32;
    let mut buf = vec![0u64; size as usize / 8];
    unsafe {
        let mut r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        if r == 111 {
            // ERROR_BUFFER_OVERFLOW: `size` now says how much is needed
            buf = vec![0u64; size as usize / 8 + 1];
            r = GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        }
        if r != 0 {
            return None;
        }
        let mut best: Option<((bool, u32), Nic)> = None;
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            let a = &*p;
            p = a.Next;
            if a.OperStatus != IfOperStatusUp || a.FirstGatewayAddress.is_null() || !(a.IfType == ETHERNET || a.IfType == WIFI) {
                continue;
            }
            let mut ip = None;
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let sa = (*u).Address.lpSockaddr;
                if !sa.is_null() && (*sa).sa_family == AF_INET {
                    let b = (*(sa as *const SOCKADDR_IN)).sin_addr.S_un.S_un_b;
                    ip = Some(Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4));
                    break;
                }
                u = (*u).Next;
            }
            let Some(ip) = ip.filter(|ip| !ip.is_link_local() && !ip.is_unspecified()) else { continue };
            let desc = a.Description.to_string().unwrap_or_default().to_lowercase();
            // Typed as Ethernet but a tunnel or a VM switch: only if nothing better exists.
            let odd = ["vpn", "tap-", "tap ", "tun", "wintun", "wireguard", "virtual", "hyper-v", "vmware", "virtualbox"]
                .iter()
                .any(|k| desc.contains(k));
            let rank = (odd, a.Ipv4Metric);
            let nic = Nic { ip, index: a.Anonymous1.Anonymous.IfIndex, name: a.FriendlyName.to_string().unwrap_or_default() };
            if best.as_ref().map_or(true, |(r, _)| rank < *r) {
                best = Some((rank, nic));
            }
        }
        best.map(|(_, n)| n)
    }
}

/// The streaming socket plus what the STUN servers say about it. Prefers sending through the real adapter,
/// past any VPN: a VPN that catches all traffic (Happ, sing-box, xray "TUN mode"...) sends our packets out
/// from its own ports or its server, so the address in our code is not where the other side's packets can
/// reach us, and the direct connection never forms. Falls back to the normal way if the adapter gets no
/// answers (a network where only the VPN leads outside). SCREENSHARE_DIRECT=0 turns the bypass off.
pub fn open_and_discover() -> Result<(UdpSocket, NatInfo)> {
    let want = std::env::var("SCREENSHARE_DIRECT").map_or(true, |v| v.trim() != "0");
    if let Some(nic) = want.then(physical_nic).flatten() {
        conn_log(&format!("Сетевая карта для трансляции: {} ({}), мимо VPN", nic.name, nic.ip));
        if let Ok(sock) = bind_socket_on(Some(&nic)) {
            let nat = stun_discover(&sock);
            if nat.public.is_some() {
                return Ok((sock, nat));
            }
            conn_log("Через сетевую карту напрямую интернета нет — пробую обычным путём");
        }
    }
    let sock = bind_socket()?;
    let nat = stun_discover(&sock);
    Ok((sock, nat))
}

pub fn primary_local_ip() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v) if !v.is_unspecified() => Some(v),
        _ => None,
    }
}

pub struct NatInfo {
    pub public: Option<SocketAddrV4>,
    /// Different servers saw different mappings: hole punching will likely fail.
    pub symmetric: bool,
    pub keepalive_server: Option<SocketAddr>,
}

fn stun_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut r = [0u8; 20];
    r[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    r[4..8].copy_from_slice(&STUN_COOKIE.to_be_bytes());
    r[8..20].copy_from_slice(txid);
    r
}

fn parse_stun_response(pkt: &[u8], txid: &[u8; 12]) -> Option<SocketAddrV4> {
    if pkt.len() < 20 || pkt[0..2] != [0x01, 0x01] || pkt[4..8] != STUN_COOKIE.to_be_bytes() || &pkt[8..20] != txid {
        return None;
    }
    let mut plain = None;
    let mut i = 20;
    while i + 4 <= pkt.len() {
        let ty = u16::from_be_bytes([pkt[i], pkt[i + 1]]);
        let len = u16::from_be_bytes([pkt[i + 2], pkt[i + 3]]) as usize;
        let v = pkt.get(i + 4..i + 4 + len)?;
        if (ty == 0x0020 || ty == 0x0001) && len >= 8 && v[1] == 0x01 {
            let mut port = u16::from_be_bytes([v[2], v[3]]);
            let mut ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            if ty == 0x0020 {
                port ^= (STUN_COOKIE >> 16) as u16;
                ip ^= STUN_COOKIE;
                return Some(SocketAddrV4::new(Ipv4Addr::from(ip), port));
            }
            plain = Some(SocketAddrV4::new(Ipv4Addr::from(ip), port));
        }
        i += 4 + ((len + 3) & !3);
    }
    plain
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("getrandom");
    b
}

pub fn random_token() -> u32 {
    u32::from_le_bytes(random_bytes::<4>())
}

/// Connection diagnostics: %LOCALAPPDATA%\screenshare\connect.log (what we learned about our address, where we
/// knocked, who answered). The only way to tell afterwards why two PCs could not reach each other.
pub fn conn_log(line: &str) {
    use std::io::Write;
    let Some(base) = std::env::var_os("LOCALAPPDATA") else { return };
    let p = std::path::PathBuf::from(base).join("screenshare").join("connect.log");
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 512 * 1024) {
        let _ = std::fs::remove_file(&p);
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // One write per line: appends are atomic, so the host and viewer of a local test don't interleave.
    let text = format!("{h:02}:{m:02}:{s:02} UTC  [{}] {line}\r\n", std::process::id());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// Addresses nobody outside our own network can send to.
fn is_local_only(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || (o[0] == 100 && (64..128).contains(&o[1]))
}

/// Neighbouring ports of the other side's public addresses (port-changing NATs, see `punch`).
fn spray_targets(peer: &[SocketAddrV4]) -> Vec<SocketAddrV4> {
    let mut v = Vec::new();
    for c in peer.iter().filter(|c| !is_local_only(c.ip())) {
        for d in -4i32..=32 {
            let p = c.port() as i32 + d;
            if d != 0 && (1024..=65535).contains(&p) {
                let a = SocketAddrV4::new(*c.ip(), p as u16);
                if !peer.contains(&a) && !v.contains(&a) {
                    v.push(a);
                }
            }
        }
    }
    v
}

/// Asks several STUN servers for our public address, using the socket we will stream on.
pub fn stun_discover(sock: &UdpSocket) -> NatInfo {
    let servers: Vec<SocketAddr> = STUN_SERVERS
        .iter()
        .filter_map(|s| s.to_socket_addrs().ok()?.find(|a| a.is_ipv4()))
        .collect();
    let txids: Vec<[u8; 12]> = servers.iter().map(|_| random_bytes::<12>()).collect();
    let mut results: Vec<Option<SocketAddrV4>> = vec![None; servers.len()];
    let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
    let start = Instant::now();
    let mut last_send: Option<Instant> = None;
    let mut buf = [0u8; 1500];
    while start.elapsed() < Duration::from_millis(2000) {
        if last_send.map_or(true, |t| t.elapsed() > Duration::from_millis(400)) {
            for (i, srv) in servers.iter().enumerate() {
                if results[i].is_none() {
                    let _ = sock.send_to(&stun_request(&txids[i]), srv);
                }
            }
            last_send = Some(Instant::now());
        }
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                for (i, tx) in txids.iter().enumerate() {
                    if let Some(a) = parse_stun_response(&buf[..n], tx) {
                        results[i] = Some(a);
                    }
                }
                if results.iter().filter(|r| r.is_some()).count() >= 3 || results.iter().all(|r| r.is_some()) {
                    break;
                }
            }
            Err(_) => {}
        }
    }
    let found: Vec<(SocketAddr, SocketAddrV4)> =
        servers.iter().zip(&results).filter_map(|(s, r)| r.map(|r| (*s, r))).collect();
    let public = found.first().map(|f| f.1);
    let symmetric = found.windows(2).any(|w| w[0].1 != w[1].1);
    let seen: Vec<String> = found.iter().map(|(s, a)| format!("{s} видит нас как {a}")).collect();
    conn_log(&format!(
        "STUN: сокет {}, ответили {} из {} серверов: {}{}",
        sock.local_addr().map_or("?".into(), |a| a.to_string()),
        found.len(),
        servers.len(),
        if seen.is_empty() { "никто".into() } else { seen.join("; ") },
        if symmetric { " — РАЗНЫЕ адреса: симметричный NAT, прямое соединение может не пройти" } else { "" },
    ));
    NatInfo { public, symmetric, keepalive_server: found.first().map(|f| f.0) }
}

/// Keeps our NAT mapping alive while a human is copying codes around.
pub fn stun_keepalive(sock: &UdpSocket, server: Option<SocketAddr>) {
    if let Some(s) = server {
        let _ = sock.send_to(&stun_request(&random_bytes::<12>()), s);
    }
}

#[derive(Clone)]
pub struct Code {
    pub role: u8,
    pub token: u32,
    pub cands: Vec<SocketAddrV4>,
    /// Present only in a host code of version 2. Not the token: the token is visible on every packet.
    pub secret: Option<[u8; 32]>,
}

const CODE_VERSION: u8 = 1;
const CODE_VERSION_KEY: u8 = 2;

impl Code {
    pub fn for_socket(role: u8, token: u32, sock: &UdpSocket, nat: &NatInfo) -> Code {
        let local = sock.local_addr().ok();
        let port = local.map_or(0, |a| a.port());
        let mut cands = Vec::new();
        // A socket bound to the real adapter: that address is the one people on the same network can reach
        // (the default route may point into a VPN tunnel, whose address is useless to them).
        let bound = match local.map(|a| a.ip()) {
            Some(IpAddr::V4(v)) if !v.is_unspecified() => Some(v),
            _ => None,
        };
        if let Some(ip) = bound.or_else(primary_local_ip) {
            cands.push(SocketAddrV4::new(ip, port));
        }
        if let Some(p) = nat.public {
            if !cands.contains(&p) {
                cands.push(p);
            }
        }
        let secret = if role == ROLE_HOST {
            let mut s = [0u8; 32];
            getrandom::fill(&mut s).expect("rng");
            Some(s)
        } else {
            None
        };
        Code { role, token, cands, secret }
    }

    pub fn encode(&self) -> String {
        let mut b = vec![if self.secret.is_some() { CODE_VERSION_KEY } else { CODE_VERSION }, self.role];
        b.extend_from_slice(&self.token.to_le_bytes());
        if let Some(s) = &self.secret {
            b.extend_from_slice(s);
        }
        for c in &self.cands {
            b.extend_from_slice(&c.ip().octets());
            b.extend_from_slice(&c.port().to_be_bytes());
        }
        let prefix = if self.role == ROLE_HOST { "H" } else { "C" };
        format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
    }

    pub fn decode(s: &str) -> Result<Code> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let body = s.get(1..).context("пустой код")?;
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .context("код повреждён — скопируйте его целиком")?;
        if b.len() < 6 {
            bail!("код повреждён — скопируйте его целиком");
        }
        let (secret, rest) = match b[0] {
            CODE_VERSION => {
                if (b.len() - 6) % 6 != 0 {
                    bail!("код повреждён — скопируйте его целиком");
                }
                (None, &b[6..])
            }
            CODE_VERSION_KEY => {
                if b.len() < 38 || (b.len() - 38) % 6 != 0 {
                    bail!("код повреждён — скопируйте его целиком");
                }
                let mut s = [0u8; 32];
                s.copy_from_slice(&b[6..38]);
                (Some(s), &b[38..])
            }
            _ => bail!("код от другой версии программы — обновитесь оба"),
        };
        let cands = rest
            .chunks(6)
            .map(|c| SocketAddrV4::new(Ipv4Addr::new(c[0], c[1], c[2], c[3]), u16::from_be_bytes([c[4], c[5]])))
            .collect();
        Ok(Code { role: b[1], token: u32::from_le_bytes(b[2..6].try_into().unwrap()), cands, secret })
    }
}

fn write_punch(out: &mut Vec<u8>, token: u32, my_role: u8, announce_enc: bool) {
    write_header(out, PT_PUNCH, token);
    out.push(my_role);
    // Second byte: this viewer can decrypt. A host from before 0.12.11 only reads the role.
    if announce_enc {
        out.push(CAP_ENC);
    }
}

/// Sends punch packets to every candidate until the peer answers.
/// Returns the address that actually works.
pub fn punch(sock: &UdpSocket, my_role: u8, token: u32, peer: &[SocketAddrV4], timeout: Duration, announce_enc: bool) -> Result<SocketAddr> {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(20)));
    let start = Instant::now();
    let mut last_send: Option<Instant> = None;
    let mut last_spray: Option<Instant> = None;
    let mut seen: Option<SocketAddr> = None;
    let mut out = Vec::with_capacity(16);
    let mut buf = [0u8; 2048];
    let sprayed = spray_targets(peer);
    let role = if my_role == ROLE_HOST { "показ" } else { "просмотр" };
    conn_log(&format!(
        "Соединение ({role}): сокет {}, адреса другой стороны: {}; соседних портов для пристрелки: {}",
        sock.local_addr().map_or("?".into(), |a| a.to_string()),
        peer.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", "),
        sprayed.len(),
    ));
    let mut heard: Vec<(SocketAddr, u8)> = Vec::new();
    let result = loop {
        if start.elapsed() > timeout {
            break Err(anyhow::anyhow!(
                "не удалось соединиться за {} с. Проверьте, что коды свежие (из этого запуска) и что программу не блокирует файрвол",
                timeout.as_secs()
            ));
        }
        if last_send.map_or(true, |t| t.elapsed() >= Duration::from_millis(100)) {
            write_punch(&mut out, token, my_role, announce_enc);
            for c in peer {
                send_media(sock, &out, SocketAddr::V4(*c));
            }
            if let Some(a) = seen {
                send_media(sock, &out, a);
            }
            last_send = Some(Instant::now());
        }
        // Many routers give every new destination its own outside port, usually the next one: the other side's
        // packets then come from a port that is not in its code, and ours never open the way for them. Knocking on
        // the neighbouring ports too opens it (and finds the right one from our side).
        if last_spray.map_or(true, |t| t.elapsed() >= Duration::from_millis(250)) {
            write_punch(&mut out, token, my_role, announce_enc);
            for a in &sprayed {
                send_media(sock, &out, SocketAddr::V4(*a));
            }
            last_spray = Some(Instant::now());
        }
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) if is_benign(&e) => continue,
            Err(e) => break Err(e.into()),
        };
        let raw = &buf[..n];
        let opened = match open_incoming(raw) {
            Incoming::Drop => continue,
            Incoming::Plain => None,
            Incoming::Opened(p) => Some(p),
        };
        let Some((ty, body)) = parse_header(opened.as_deref().unwrap_or(raw), token) else { continue };
        if !heard.iter().any(|(a, t)| *a == from && *t == ty) {
            heard.push((from, ty));
            let known = matches!(from, SocketAddr::V4(v) if peer.contains(&v));
            conn_log(&format!(
                "  +{:.1} с: пакет 0x{ty:02X} от {from}{}",
                start.elapsed().as_secs_f64(),
                if known { "" } else { " (этого адреса не было в коде — роутер другой стороны сменил порт)" }
            ));
        }
        match ty {
            PT_PUNCH | PT_PUNCH_ACK => {
                if body.first().copied() == Some(my_role) {
                    continue; // our own packet looped back
                }
                // Viewer on 0.12.11 puts CAP_ENC after its role. Old hosts ignore the extra byte.
                if ty == PT_PUNCH && body.get(1).copied() == Some(CAP_ENC) {
                    absorb_media_pair(token);
                    note_peer_enc(token);
                }
                if ty == PT_PUNCH {
                    write_header(&mut out, PT_PUNCH_ACK, token);
                    out.push(my_role);
                    send_media(sock, &out, from);
                    seen = Some(from);
                } else {
                    // Tell the peer a few more times in case our ACKs got lost.
                    write_header(&mut out, PT_PUNCH_ACK, token);
                    out.push(my_role);
                    for _ in 0..3 {
                        send_media(sock, &out, from);
                    }
                    break Ok(from);
                }
            }
            // The other side already decided we're connected and started talking.
            PT_CAPS => {
                let flags = body.first().copied().unwrap_or(0);
                if flags & CAP_ENC != 0 {
                    absorb_media_pair(token);
                    note_peer_enc(token);
                }
                break Ok(from);
            }
            PT_VIDEO | PT_AUDIO | PT_AUDIO2 | PT_AUDIO3 | PT_KEYREQ | PT_PING | PT_PONG | PT_NACK | PT_REPORT | PT_ANACK
            | PT_VIDEO_ENC | PT_AUDIO_ENC | PT_AUDIO2_ENC => break Ok(from),
            _ => {}
        }
    };
    match &result {
        Ok(a) => conn_log(&format!("  соединено с {a} за {:.1} с", start.elapsed().as_secs_f64())),
        Err(e) => conn_log(&format!(
            "  НЕ соединено за {:.0} с: {e}. {}",
            start.elapsed().as_secs_f64(),
            if heard.is_empty() {
                "От другой стороны не пришло НИ ОДНОГО пакета: её пакеты не доходят (VPN, файрвол, провайдер) или она не стучится"
            } else {
                "Пакеты приходили, но ответы не дошли до другой стороны"
            }
        )),
    }
    result
}

/// Answers stray punch packets once a session is running (the peer may still be punching).
pub fn answer_punch(sock: &UdpSocket, token: u32, my_role: u8, body: &[u8], from: SocketAddr) {
    if body.first().copied() != Some(my_role) {
        let mut out = Vec::with_capacity(8);
        write_header(&mut out, PT_PUNCH_ACK, token);
        out.push(my_role);
        send_media(sock, &out, from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spray_covers_neighbouring_public_ports_only() {
        let peer = vec!["192.168.1.5:5000".parse().unwrap(), "1.2.3.4:61000".parse().unwrap()];
        let t = spray_targets(&peer);
        assert!(t.iter().all(|a| *a.ip() == Ipv4Addr::new(1, 2, 3, 4)), "never sprays private addresses");
        assert!(t.contains(&"1.2.3.4:61001".parse().unwrap()) && t.contains(&"1.2.3.4:60996".parse().unwrap()));
        assert!(!t.contains(&"1.2.3.4:61000".parse().unwrap()), "the exact port is knocked on anyway");
        assert_eq!(t.len(), 36);
        // CGNAT (100.64/10) addresses are not reachable from outside either.
        assert!(spray_targets(&["100.70.1.1:4000".parse().unwrap()]).is_empty());
    }

    #[test]
    fn sealed_media_roundtrips_and_plaintext_stays_plaintext() {
        let token = 0xA1B2_C3D4u32;
        if let Ok(mut g) = media_arm().lock() {
            *g = MediaArm::default();
        }
        install_session_key(&[3u8; 32], token, ROLE_CLIENT);
        install_media_key(&[9u8; 32], token);
        let mut video = vec![PT_VIDEO];
        video.extend_from_slice(&token.to_le_bytes());
        video.extend_from_slice(&7u32.to_le_bytes());
        video.extend_from_slice(&1u16.to_le_bytes());
        video.extend_from_slice(&4u16.to_le_bytes());
        video.push(1);
        video.extend_from_slice(b"nal");
        let sealed = seal_media(&armed_media_key().unwrap(), &video).unwrap();
        assert_eq!(sealed[0], PT_VIDEO_ENC);
        assert!(sealed.len() > video.len());
        match open_incoming(&sealed) {
            Incoming::Opened(p) => assert_eq!(p, video),
            Incoming::Plain => panic!("plaintext"),
            Incoming::Drop => panic!("dropped"),
        }
        let mut audio = vec![PT_AUDIO3];
        audio.extend_from_slice(&token.to_le_bytes());
        audio.extend_from_slice(&9u32.to_le_bytes());
        audio.extend_from_slice(b"op");
        let a = seal_media(&armed_media_key().unwrap(), &audio).unwrap();
        assert_eq!(a[0], PT_AUDIO_ENC);
        match open_incoming(&a) {
            Incoming::Opened(p) => assert_eq!(p, audio),
            Incoming::Plain => panic!("plaintext"),
            Incoming::Drop => panic!("dropped"),
        }
        // Disconnect and feedback are authenticated, not just media.
        for ty in [PT_PING, PT_BYE, PT_CAPS, PT_REPORT, PT_PUNCH, PT_PUNCH_ACK, PT_NACK, PT_ANACK] {
            media_arm().lock().unwrap().role = ROLE_CLIENT;
            let pkt = [ty, 0, 0, 0, 0];
            let sealed = seal_media(&armed_media_key().unwrap(), &pkt).unwrap();
            assert!(matches!(open_incoming(&sealed), Incoming::Drop), "reflection");
            media_arm().lock().unwrap().role = ROLE_HOST;
            let mut forged = sealed.clone();
            *forged.last_mut().unwrap() ^= 1;
            assert!(matches!(open_incoming(&forged), Incoming::Drop), "bad tag must not advance replay window");
            assert!(matches!(open_incoming(&sealed), Incoming::Opened(p) if p == pkt));
            assert!(matches!(open_incoming(&sealed), Incoming::Drop), "replay");
        }
        media_arm().lock().unwrap().role = ROLE_CLIENT;
        let mut input = vec![PT_INPUT];
        input.extend_from_slice(&token.to_le_bytes());
        input.extend_from_slice(&[1, 0, 1]);
        let sealed_in = seal_media(&armed_media_key().unwrap(), &input).unwrap();
        assert_eq!(sealed_in[0], PT_INPUT_ENC);
        media_arm().lock().unwrap().role = ROLE_HOST;
        match open_incoming(&sealed_in) {
            Incoming::Opened(p) => assert_eq!(p, input),
            Incoming::Plain => panic!("plaintext"),
            Incoming::Drop => panic!("dropped"),
        }
        // The token is visible in every header. A clear input packet must not be accepted once the
        // session key exists, and a packet sealed with the session key must still open after the
        // friend key takes over sending.
        let mut clear = vec![PT_INPUT, 0, 0, 0, 0, 1];
        clear[1..5].copy_from_slice(&token.to_le_bytes());
        assert!(matches!(open_incoming(&clear), Incoming::Drop));
        assert!(matches!(open_incoming(&[PT_PING, 0, 0, 0, 0]), Incoming::Drop));
        let session_only = {
            let g = media_arm().lock().unwrap();
            g.session.unwrap()
        };
        media_arm().lock().unwrap().role = ROLE_CLIENT;
        let sealed_session = seal_media(&session_only, &input).unwrap();
        media_arm().lock().unwrap().role = ROLE_HOST;
        match open_incoming(&sealed_session) {
            Incoming::Opened(p) => assert_eq!(p, input),
            Incoming::Plain => panic!("plaintext"),
            Incoming::Drop => panic!("dropped"),
        }
        // Counter exhaustion fails closed, including at the actual UDP boundary.
        media_arm().lock().unwrap().ctrl_nonce = u64::MAX;
        assert!(seal_media(&session_only, &input).is_none());
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        recv.set_read_timeout(Some(Duration::from_millis(30))).unwrap();
        let send = UdpSocket::bind("127.0.0.1:0").unwrap();
        send_media(&send, &input, recv.local_addr().unwrap());
        assert!(recv.recv_from(&mut [0u8; 128]).is_err(), "encryption failure must not leak plaintext");
    }

    #[test]
    fn code_roundtrip() {
        let c = Code {
            role: ROLE_HOST,
            token: 0xDEADBEEF,
            cands: vec!["192.168.1.5:5000".parse().unwrap(), "1.2.3.4:61000".parse().unwrap()],
            secret: None,
        };
        let s = c.encode();
        assert!(s.starts_with('H'));
        let d = Code::decode(&format!("  {s}\n")).unwrap();
        assert_eq!(d.role, ROLE_HOST);
        assert_eq!(d.token, 0xDEADBEEF);
        assert_eq!(d.cands, c.cands);
        assert!(d.secret.is_none());
        let mut keyed = c.clone();
        keyed.secret = Some([9u8; 32]);
        let s2 = keyed.encode();
        let d2 = Code::decode(&s2).unwrap();
        assert_eq!(d2.secret, Some([9u8; 32]));
        assert_eq!(d2.cands, c.cands);
        assert_eq!(d2.token, c.token);
    }

    #[test]
    fn stun_parse_xor_mapped() {
        let tx = [7u8; 12];
        let mut p = vec![0x01, 0x01, 0, 12];
        p.extend_from_slice(&STUN_COOKIE.to_be_bytes());
        p.extend_from_slice(&tx);
        p.extend_from_slice(&[0x00, 0x20, 0, 8, 0, 0x01]);
        p.extend_from_slice(&(5000u16 ^ 0x2112).to_be_bytes());
        p.extend_from_slice(&(u32::from(Ipv4Addr::new(1, 2, 3, 4)) ^ STUN_COOKIE).to_be_bytes());
        assert_eq!(parse_stun_response(&p, &tx), Some("1.2.3.4:5000".parse().unwrap()));
    }
}
