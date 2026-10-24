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
pub const PT_AUDIO: u8 = 0x91;
pub const PT_KEYREQ: u8 = 0xA0;
pub const PT_PING: u8 = 0xA1;
pub const PT_PONG: u8 = 0xA2;
pub const PT_BYE: u8 = 0xA3;

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

pub fn bind_socket() -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Big buffers matter: a keyframe is hundreds of datagrams arriving back to back.
    let _ = s.set_recv_buffer_size(16 << 20);
    let _ = s.set_send_buffer_size(8 << 20);
    s.bind(&SocketAddr::from(([0, 0, 0, 0], 0)).into())?;
    Ok(s.into())
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
    NatInfo { public, symmetric, keepalive_server: found.first().map(|f| f.0) }
}

/// Keeps our NAT mapping alive while a human is copying codes around.
pub fn stun_keepalive(sock: &UdpSocket, server: Option<SocketAddr>) {
    if let Some(s) = server {
        let _ = sock.send_to(&stun_request(&random_bytes::<12>()), s);
    }
}

pub struct Code {
    pub role: u8,
    pub token: u32,
    pub cands: Vec<SocketAddrV4>,
}

const CODE_VERSION: u8 = 1;

impl Code {
    pub fn for_socket(role: u8, token: u32, sock: &UdpSocket, nat: &NatInfo) -> Code {
        let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
        let mut cands = Vec::new();
        if let Some(ip) = primary_local_ip() {
            cands.push(SocketAddrV4::new(ip, port));
        }
        if let Some(p) = nat.public {
            if !cands.contains(&p) {
                cands.push(p);
            }
        }
        Code { role, token, cands }
    }

    pub fn encode(&self) -> String {
        let mut b = vec![CODE_VERSION, self.role];
        b.extend_from_slice(&self.token.to_le_bytes());
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
            .context("код повреждён — скопируй его целиком")?;
        if b.len() < 6 || (b.len() - 6) % 6 != 0 {
            bail!("код повреждён — скопируй его целиком");
        }
        if b[0] != CODE_VERSION {
            bail!("код от другой версии программы — обновитесь оба");
        }
        let cands = b[6..]
            .chunks(6)
            .map(|c| SocketAddrV4::new(Ipv4Addr::new(c[0], c[1], c[2], c[3]), u16::from_be_bytes([c[4], c[5]])))
            .collect();
        Ok(Code { role: b[1], token: u32::from_le_bytes(b[2..6].try_into().unwrap()), cands })
    }
}

/// Sends punch packets to every candidate until the peer answers.
/// Returns the address that actually works.
pub fn punch(sock: &UdpSocket, my_role: u8, token: u32, peer: &[SocketAddrV4], timeout: Duration) -> Result<SocketAddr> {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(20)));
    let start = Instant::now();
    let mut last_send: Option<Instant> = None;
    let mut seen: Option<SocketAddr> = None;
    let mut out = Vec::with_capacity(16);
    let mut buf = [0u8; 2048];
    loop {
        if start.elapsed() > timeout {
            bail!(
                "не удалось соединиться за {} с. Проверьте, что коды свежие (из этого запуска) и что программу не блокирует файрвол",
                timeout.as_secs()
            );
        }
        if last_send.map_or(true, |t| t.elapsed() >= Duration::from_millis(100)) {
            write_header(&mut out, PT_PUNCH, token);
            out.push(my_role);
            for c in peer {
                let _ = sock.send_to(&out, SocketAddr::V4(*c));
            }
            if let Some(a) = seen {
                let _ = sock.send_to(&out, a);
            }
            last_send = Some(Instant::now());
        }
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) if is_benign(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        let Some((ty, body)) = parse_header(&buf[..n], token) else { continue };
        match ty {
            PT_PUNCH | PT_PUNCH_ACK => {
                if body.first().copied() == Some(my_role) {
                    continue; // our own packet looped back
                }
                if ty == PT_PUNCH {
                    write_header(&mut out, PT_PUNCH_ACK, token);
                    out.push(my_role);
                    let _ = sock.send_to(&out, from);
                    seen = Some(from);
                } else {
                    // Tell the peer a few more times in case our ACKs got lost.
                    write_header(&mut out, PT_PUNCH_ACK, token);
                    out.push(my_role);
                    for _ in 0..3 {
                        let _ = sock.send_to(&out, from);
                    }
                    return Ok(from);
                }
            }
            // The other side already decided we're connected and started talking.
            PT_VIDEO | PT_AUDIO | PT_KEYREQ | PT_PING | PT_PONG => return Ok(from),
            _ => {}
        }
    }
}

/// Answers stray punch packets once a session is running (the peer may still be punching).
pub fn answer_punch(sock: &UdpSocket, token: u32, my_role: u8, body: &[u8], from: SocketAddr) {
    if body.first().copied() != Some(my_role) {
        let mut out = Vec::with_capacity(8);
        write_header(&mut out, PT_PUNCH_ACK, token);
        out.push(my_role);
        let _ = sock.send_to(&out, from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrip() {
        let c = Code {
            role: ROLE_HOST,
            token: 0xDEADBEEF,
            cands: vec!["192.168.1.5:5000".parse().unwrap(), "1.2.3.4:61000".parse().unwrap()],
        };
        let s = c.encode();
        assert!(s.starts_with('H'));
        let d = Code::decode(&format!("  {s}\n")).unwrap();
        assert_eq!(d.role, ROLE_HOST);
        assert_eq!(d.token, 0xDEADBEEF);
        assert_eq!(d.cands, c.cands);
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
