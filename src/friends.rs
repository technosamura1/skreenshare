//! Friends, the Parsec way, on top of the same public ntfy relay as `easy.rs` — no server of ours.
//!
//! Each install has a random id and inbox key (created once, kept next to gui.ini). The friend code / link
//! is that pair: it names an unguessable inbox topic and is the only key that opens it. Adding someone posts
//! one sealed hello there; both sides then share a pair topic. The pair secret is derived from the two
//! inbox keys, so it is the same no matter who pastes the code, or if both do. A stranger who never saw
//! either key cannot find the topic or read it.
//!
//! This file is pure logic (std + the crypto crates). HTTP stays in `ntfy.rs`, the window in `gui.rs`.
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Same alphabet as the short room codes, so a code can be read aloud if it has to be.
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub const KIND_HELLO: u8 = b'H';
pub const KIND_PRESENCE: u8 = b'P';
pub const KIND_INVITE: u8 = b'I';
pub const KIND_ANSWER: u8 = b'Y';
pub const KIND_DECLINE: u8 = b'N';

/// How long an invitation stays tappable.
pub const INVITE_TTL_SECS: u64 = 15 * 60;
/// At least this often we would like a presence beat, when the relay budget allows it.
pub const PRESENCE_WANT_SECS: u64 = 45;
/// ntfy.sh: after a burst, one request per 5 s. Never schedule faster than that.
pub const RELAY_MIN_GAP_MS: u64 = 5_000;
pub const MAX_FRIENDS: usize = 32;
const NAME_MAX: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id: [u8; 16],
    pub key: [u8; 32],
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Friend {
    pub id: [u8; 16],
    /// Inbox key from their friend code (or from their hello). A later hello must show the same key.
    pub their_key: [u8; 32],
    pub pair: [u8; 32],
    pub name: String,
    /// The user typed this name; presence from the other side must not overwrite it.
    pub renamed: bool,
}

#[derive(Clone, Debug)]
pub struct Book {
    pub me: Identity,
    pub friends: Vec<Friend>,
}

/// A sealed blob ready to POST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutMsg {
    pub topic: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InviteKind {
    /// Short room code: the viewer starts with `--room` and `easy.rs` finishes the P2P exchange.
    Room,
    /// The host's ordinary H… code. The viewer's C… answer comes back as `KIND_ANSWER`.
    Offer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub kind: InviteKind,
    pub code: String,
    pub unix: u64,
    pub nonce: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairEvent {
    Presence { unix: u64, name: String },
    Invite(Invite),
    Answer { nonce: String, code: String },
    Decline { nonce: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddError {
    BadCode,
    OwnCode,
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayLine {
    pub id: String,
    pub time: u64,
    pub message: String,
}

struct Channel {
    topic: String,
    key: [u8; 32],
}

impl Identity {
    pub fn generate(name: &str) -> Identity {
        let mut id = [0u8; 16];
        let mut key = [0u8; 32];
        getrandom::fill(&mut id).expect("rng");
        getrandom::fill(&mut key).expect("rng");
        Identity { id, key, name: sanitize_name(name) }
    }
}

impl Book {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(me: Identity) -> Book {
        Book { me, friends: Vec::new() }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn generate(name: &str) -> Book {
        Book::new(Identity::generate(name))
    }

    pub fn code(&self) -> String {
        encode_code(&self.me.id, &self.me.key)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn link(&self) -> String {
        format!("screenshare://friend/{}", encode_code_raw(&self.me.id, &self.me.key))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_my_name(&mut self, name: &str) {
        self.me.name = sanitize_name(name);
    }

    /// Remember `code`'s owner and seal a hello into their inbox. A new friend gets the pair secret
    /// derived from both inbox keys ([`pair_secret`]), so both people can paste the code and still
    /// land on one channel. A friend already in the list keeps the secret they were added with:
    /// 0.12.8 stored a random one, and replacing it would split the channel. Adding again resends
    /// that same secret. A name the user edited is kept.
    pub fn add(&mut self, code: &str) -> Result<OutMsg, AddError> {
        let (id, key) = parse_code(code).ok_or(AddError::BadCode)?;
        if id == self.me.id {
            return Err(AddError::OwnCode);
        }
        if self.friends.len() >= MAX_FRIENDS && self.friends.iter().all(|f| f.id != id) {
            return Err(AddError::Full);
        }
        if let Some(f) = self.friends.iter().find(|f| f.id == id) {
            if f.their_key != key {
                return Err(AddError::BadCode);
            }
            let pair = f.pair;
            return Ok(seal_hello(&self.me, &id, &key, &pair));
        }
        let pair = pair_secret(&self.me.id, &self.me.key, &id, &key);
        self.friends.push(Friend {
            id,
            their_key: key,
            pair,
            name: "Друг".into(),
            renamed: false,
        });
        Ok(seal_hello(&self.me, &id, &key, &pair))
    }

    pub fn rename(&mut self, id: &[u8; 16], name: &str) -> bool {
        let Some(f) = self.friends.iter_mut().find(|f| &f.id == id) else { return false };
        f.name = sanitize_name(name);
        f.renamed = true;
        true
    }

    pub fn remove(&mut self, id: &[u8; 16]) -> bool {
        let n = self.friends.len();
        self.friends.retain(|f| &f.id != id);
        self.friends.len() != n
    }

    /// Hellos addressed to us. A hello whose id we already know is applied only when the inbox key matches.
    /// An existing pair is kept, unless the hello carries the derived secret ([`pair_secret`]): that one
    /// replaces a leftover random secret from 0.12.8. A random hello never overwrites a pair we already have.
    pub fn take_hellos(&mut self, messages: &[String]) -> usize {
        let ch = inbox_channel(&self.me.id, &self.me.key);
        let mut n = 0;
        for m in messages {
            let Some(body) = ch.open(KIND_HELLO, m) else { continue };
            let Some((id, key, pair, name)) = parse_hello(&body) else { continue };
            if id == self.me.id || self.friends.len() >= MAX_FRIENDS && self.friends.iter().all(|f| f.id != id) {
                continue;
            }
            if let Some(f) = self.friends.iter_mut().find(|f| f.id == id) {
                if f.their_key != key {
                    continue;
                }
                let canonical = pair_secret(&self.me.id, &self.me.key, &id, &key);
                if f.pair != pair && pair == canonical {
                    f.pair = pair;
                }
                if !f.renamed {
                    f.name = name;
                }
            } else {
                self.friends.push(Friend { id, their_key: key, pair, name, renamed: false });
            }
            n += 1;
        }
        n
    }

    pub fn presence(&self, id: &[u8; 16], unix: u64) -> Option<OutMsg> {
        let f = self.friends.iter().find(|f| &f.id == id)?;
        let ch = pair_channel(&self.me.id, &f.id, &f.pair);
        let body = format!("{unix}\n{}", self.me.name);
        Some(pack(ch, KIND_PRESENCE, &body))
    }

    pub fn invite(&self, id: &[u8; 16], kind: InviteKind, code: &str, unix: u64) -> Option<OutMsg> {
        let f = self.friends.iter().find(|f| &f.id == id)?;
        let ch = pair_channel(&self.me.id, &f.id, &f.pair);
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).expect("rng");
        let tag = match kind {
            InviteKind::Room => "room",
            InviteKind::Offer => "offer",
        };
        let body = format!("{tag}\n{code}\n{unix}\n{}\n{}", hex(&nonce), self.me.name);
        Some(pack(ch, KIND_INVITE, &body))
    }

    pub fn answer(&self, id: &[u8; 16], nonce: &str, code: &str) -> Option<OutMsg> {
        self.pair_line(id, KIND_ANSWER, &format!("{nonce}\n{code}"))
    }

    pub fn decline(&self, id: &[u8; 16], nonce: &str) -> Option<OutMsg> {
        self.pair_line(id, KIND_DECLINE, nonce)
    }

    fn pair_line(&self, id: &[u8; 16], kind: u8, body: &str) -> Option<OutMsg> {
        let f = self.friends.iter().find(|f| &f.id == id)?;
        let ch = pair_channel(&self.me.id, &f.id, &f.pair);
        Some(pack(ch, kind, body))
    }

    /// One sealed pair-topic message from `id`, if it is theirs. Also learns a presence name.
    pub fn open_pair(&mut self, id: &[u8; 16], message: &str) -> Option<PairEvent> {
        let f = self.friends.iter().find(|f| &f.id == id)?;
        let ch = pair_channel(&self.me.id, &f.id, &f.pair);
        let (kind, body) = ch.open_any(message)?;
        let ev = parse_pair(kind, &body)?;
        if let PairEvent::Presence { name, .. } = &ev {
            if let Some(f) = self.friends.iter_mut().find(|f| &f.id == id) {
                if !f.renamed && !name.is_empty() {
                    f.name = name.clone();
                }
            }
        }
        Some(ev)
    }

    pub fn inbox_topic(&self) -> String {
        inbox_channel(&self.me.id, &self.me.key).topic
    }

    pub fn pair_topic(&self, id: &[u8; 16]) -> Option<String> {
        let f = self.friends.iter().find(|f| &f.id == id)?;
        Some(pair_channel(&self.me.id, &f.id, &f.pair).topic)
    }
}

pub fn add_error_text(e: AddError) -> &'static str {
    match e {
        AddError::BadCode => "Это не код друга. Нужна ссылка или код из его окна «Друзья».",
        AddError::OwnCode => "Это ваш собственный код. Нужен код другого компьютера.",
        AddError::Full => "Список друзей заполнен (32). Удалите кого-нибудь и добавьте снова.",
    }
}

/// `last` is the unix time of their newest presence. `n_friends` is how wide the relay schedule is:
/// a long list cannot beat the ntfy limit, so "offline" waits a little longer before it flips.
pub fn is_online(last: Option<u64>, now: u64, n_friends: usize) -> bool {
    let Some(t) = last else { return false };
    now.saturating_sub(t) <= online_ttl_secs(n_friends)
}

pub fn online_ttl_secs(n_friends: usize) -> u64 {
    cycle_secs(n_friends).saturating_mul(2).saturating_add(15)
}

/// Seconds one full pass takes (inbox poll + a poll and a presence post per friend), never faster
/// than the relay allows and never faster than [`PRESENCE_WANT_SECS`] when the list is short.
pub fn cycle_secs(n_friends: usize) -> u64 {
    let ops = ops_per_cycle(n_friends);
    let min = ops.saturating_mul(RELAY_MIN_GAP_MS / 1000);
    min.max(PRESENCE_WANT_SECS)
}

pub fn ops_per_cycle(n_friends: usize) -> u64 {
    1 + 2 * n_friends as u64
}

/// Gap between two relay calls for a list of this size. Always ≥ 5 s.
pub fn relay_gap_ms(n_friends: usize) -> u64 {
    let ops = ops_per_cycle(n_friends).max(1);
    let gap = cycle_secs(n_friends).saturating_mul(1000) / ops;
    gap.max(RELAY_MIN_GAP_MS)
}

pub fn invite_fresh(unix: u64, now: u64) -> bool {
    now.saturating_sub(unix) <= INVITE_TTL_SECS && unix <= now.saturating_add(60)
}

/// `{"id":"..","time":1,"event":"message","message":"<base64url>"}` lines, same shape `easy.rs` accepts.
pub fn parse_relay(body: &str) -> Vec<RelayLine> {
    let mut out = Vec::new();
    for line in body.lines() {
        if !line.contains("\"event\":\"message\"") {
            continue;
        }
        let Some(message) = json_string(line, "message") else { continue };
        if message.is_empty() || !message.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            continue;
        }
        let id = json_string(line, "id").unwrap_or("").to_string();
        let time = json_u64(line, "time").unwrap_or(0);
        out.push(RelayLine { id, time, message: message.to_string() });
    }
    out
}

/// The `since=` cursor for the next poll: the last message id, else whatever we had.
pub fn next_since<'a>(prev: &'a str, lines: &'a [RelayLine]) -> &'a str {
    lines.iter().rev().find(|l| !l.id.is_empty()).map(|l| l.id.as_str()).unwrap_or(prev)
}

// ---------------------------------------------------------------- profile folder (next to gui.ini)

/// `%LOCALAPPDATA%\screenshare`, or `SCREENSHARE_PROFILE` when set (two copies on one PC).
pub fn profile_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SCREENSHARE_PROFILE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    match std::env::var_os("LOCALAPPDATA") {
        Some(base) => PathBuf::from(base).join("screenshare"),
        None => PathBuf::from("screenshare"),
    }
}

pub fn load_book(dir: &Path) -> Book {
    let me = read_identity(&dir.join("identity.txt")).unwrap_or_else(|| {
        let me = Identity::generate(&default_name());
        let _ = write_identity(&dir.join("identity.txt"), &me);
        me
    });
    let friends = read_friends(&dir.join("friends.txt"));
    Book { me, friends }
}

pub fn save_book(dir: &Path, book: &Book) {
    let _ = std::fs::create_dir_all(dir);
    let _ = write_identity(&dir.join("identity.txt"), &book.me);
    let _ = write_friends(&dir.join("friends.txt"), &book.friends);
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Компьютер".into())
}

// ---------------------------------------------------------------- code (48 bytes → grouped base32)

fn encode_code(id: &[u8; 16], key: &[u8; 32]) -> String {
    let raw = encode_code_raw(id, key);
    raw.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("-")
}

fn encode_code_raw(id: &[u8; 16], key: &[u8; 32]) -> String {
    let mut bytes = [0u8; 48];
    bytes[..16].copy_from_slice(id);
    bytes[16..].copy_from_slice(key);
    let mut out = String::new();
    let mut acc = 0u32;
    let mut n = 0;
    for b in bytes {
        acc = (acc << 8) | b as u32;
        n += 8;
        while n >= 5 {
            n -= 5;
            out.push(ALPHABET[((acc >> n) & 31) as usize] as char);
        }
    }
    if n > 0 {
        out.push(ALPHABET[((acc << (5 - n)) & 31) as usize] as char);
    }
    out
}

/// Friend code, with or without dashes, or a `screenshare://friend/…` link.
pub fn parse_code(input: &str) -> Option<([u8; 16], [u8; 32])> {
    let s = input.trim();
    let s = s.rsplit("friend/").next().unwrap_or(s);
    let s: String = s.chars().filter(|c| !c.is_whitespace() && *c != '-' && *c != '_').collect::<String>().to_ascii_uppercase();
    if s.is_empty() || !s.bytes().all(|b| ALPHABET.contains(&b)) {
        return None;
    }
    let mut acc = 0u32;
    let mut n = 0;
    let mut bytes = Vec::with_capacity(48);
    for c in s.bytes() {
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        acc = (acc << 5) | v;
        n += 5;
        if n >= 8 {
            n -= 8;
            bytes.push((acc >> n) as u8);
        }
    }
    if bytes.len() != 48 {
        return None;
    }
    let mut id = [0u8; 16];
    let mut key = [0u8; 32];
    id.copy_from_slice(&bytes[..16]);
    key.copy_from_slice(&bytes[16..]);
    Some((id, key))
}

pub fn sanitize_name(name: &str) -> String {
    let t: String = name.chars().filter(|c| !c.is_control()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.chars().take(NAME_MAX).collect::<String>().trim().to_string();
    if t.is_empty() { "Друг".into() } else { t }
}

// ---------------------------------------------------------------- channels

fn kdf(label: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(label.as_bytes());
    for p in parts {
        h.update(p);
    }
    let d = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

fn pair_secret(a_id: &[u8; 16], a_key: &[u8; 32], b_id: &[u8; 16], b_key: &[u8; 32]) -> [u8; 32] {
    let (lo_id, lo_key, hi_id, hi_key) = if a_id <= b_id {
        (a_id.as_slice(), a_key.as_slice(), b_id.as_slice(), b_key.as_slice())
    } else {
        (b_id.as_slice(), b_key.as_slice(), a_id.as_slice(), a_key.as_slice())
    };
    kdf("screenshare-friend-pair-secret-v1", &[lo_id, lo_key, hi_id, hi_key])
}

fn inbox_channel(id: &[u8; 16], key: &[u8; 32]) -> Channel {
    let topic_m = kdf("screenshare-friend-inbox-topic-v1", &[id, key]);
    let key = kdf("screenshare-friend-inbox-key-v1", &[id, key]);
    Channel { topic: format!("ssf{}", hex(&topic_m[..16])), key }
}

fn pair_channel(a: &[u8; 16], b: &[u8; 16], secret: &[u8; 32]) -> Channel {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let topic_m = kdf("screenshare-friend-pair-topic-v1", &[lo, hi, secret]);
    let key = kdf("screenshare-friend-pair-key-v1", &[lo, hi, secret]);
    Channel { topic: format!("ssp{}", hex(&topic_m[..16])), key }
}

fn pack(ch: Channel, kind: u8, body: &str) -> OutMsg {
    let body = ch.seal(kind, body);
    OutMsg { topic: ch.topic, body }
}

fn seal_hello(me: &Identity, their_id: &[u8; 16], their_key: &[u8; 32], pair: &[u8; 32]) -> OutMsg {
    let ch = inbox_channel(their_id, their_key);
    let body = format!("v1\n{}\n{}\n{}\n{}", hex(&me.id), hex(&me.key), hex(pair), me.name);
    OutMsg { topic: ch.topic.clone(), body: ch.seal(KIND_HELLO, &body) }
}

fn parse_hello(body: &str) -> Option<([u8; 16], [u8; 32], [u8; 32], String)> {
    let mut it = body.split('\n');
    if it.next()? != "v1" {
        return None;
    }
    let id = unhex16(it.next()?)?;
    let key = unhex32(it.next()?)?;
    let pair = unhex32(it.next()?)?;
    let name = sanitize_name(it.next().unwrap_or("Друг"));
    Some((id, key, pair, name))
}

fn parse_pair(kind: u8, body: &str) -> Option<PairEvent> {
    match kind {
        KIND_PRESENCE => {
            let (unix, name) = body.split_once('\n')?;
            let unix: u64 = unix.parse().ok()?;
            Some(PairEvent::Presence { unix, name: sanitize_name(name) })
        }
        KIND_INVITE => {
            let mut it = body.split('\n');
            let kind = match it.next()? {
                "room" => InviteKind::Room,
                "offer" => InviteKind::Offer,
                _ => return None,
            };
            let code = it.next()?.trim();
            if code.is_empty() || code.len() > 4000 || code.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return None;
            }
            let unix: u64 = it.next()?.parse().ok()?;
            let nonce = it.next()?.trim();
            if nonce.len() != 16 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let name = sanitize_name(it.next().unwrap_or("Друг"));
            Some(PairEvent::Invite(Invite { kind, code: code.to_string(), unix, nonce: nonce.to_ascii_lowercase(), name }))
        }
        KIND_ANSWER => {
            let (nonce, code) = body.split_once('\n')?;
            if nonce.len() != 16 || code.is_empty() || code.len() > 4000 {
                return None;
            }
            Some(PairEvent::Answer { nonce: nonce.to_ascii_lowercase(), code: code.to_string() })
        }
        KIND_DECLINE => {
            let nonce = body.trim();
            if nonce.len() != 16 {
                return None;
            }
            Some(PairEvent::Decline { nonce: nonce.to_ascii_lowercase() })
        }
        _ => None,
    }
}

impl Channel {
    fn aad(&self, kind: u8) -> Vec<u8> {
        let mut a = self.topic.as_bytes().to_vec();
        a.push(kind);
        a
    }

    fn seal(&self, kind: u8, body: &str) -> String {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("rng");
        let mut plain = vec![kind];
        plain.extend_from_slice(body.as_bytes());
        let c = ChaCha20Poly1305::new_from_slice(&self.key).expect("key");
        let ct = c.encrypt(&Nonce::from(nonce), Payload { msg: &plain, aad: &self.aad(kind) }).expect("seal");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        b64(&out)
    }

    fn open(&self, kind: u8, message: &str) -> Option<String> {
        let raw = unb64(message.trim())?;
        if raw.len() < 12 + 16 + 2 {
            return None;
        }
        let nonce: [u8; 12] = raw[..12].try_into().ok()?;
        let c = ChaCha20Poly1305::new_from_slice(&self.key).ok()?;
        let plain = c.decrypt(&Nonce::from(nonce), Payload { msg: &raw[12..], aad: &self.aad(kind) }).ok()?;
        if plain.first() != Some(&kind) {
            return None;
        }
        String::from_utf8(plain[1..].to_vec()).ok()
    }

    fn open_any(&self, message: &str) -> Option<(u8, String)> {
        for k in [KIND_PRESENCE, KIND_INVITE, KIND_ANSWER, KIND_DECLINE, KIND_HELLO] {
            if let Some(body) = self.open(k, message) {
                return Some((k, body));
            }
        }
        None
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str, n: usize) -> Option<Vec<u8>> {
    if s.len() != n * 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..n).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}

fn unhex16(s: &str) -> Option<[u8; 16]> {
    unhex(s, 16)?.try_into().ok()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    unhex(s, 32)?.try_into().ok()
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()
}

fn json_string<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":\"");
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let j = rest.find('"')?;
    Some(&rest[..j])
}

fn json_u64(line: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let i = line.find(&pat)?;
    let rest = line[i + pat.len()..].trim_start();
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() { None } else { num.parse().ok() }
}

fn enc_name(s: &str) -> String {
    let mut o = String::new();
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(*b, b'-' | b'_' | b'.') {
            o.push(*b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

fn dec_name(s: &str) -> String {
    let b = s.as_bytes();
    let mut o = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                o.push(v);
                i += 3;
                continue;
            }
        }
        o.push(b[i]);
        i += 1;
    }
    String::from_utf8(o).unwrap_or_else(|_| "Друг".into())
}

fn write_identity(path: &Path, me: &Identity) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let text = format!("v1\nid {}\nkey {}\nname {}\n", hex(&me.id), hex(&me.key), enc_name(&me.name));
    std::fs::write(path, text)
}

fn read_identity(path: &Path) -> Option<Identity> {
    let t = std::fs::read_to_string(path).ok()?;
    let mut id = None;
    let mut key = None;
    let mut name = None;
    for l in t.lines() {
        if let Some(v) = l.strip_prefix("id ") {
            id = unhex16(v.trim());
        } else if let Some(v) = l.strip_prefix("key ") {
            key = unhex32(v.trim());
        } else if let Some(v) = l.strip_prefix("name ") {
            name = Some(dec_name(v.trim()));
        }
    }
    Some(Identity { id: id?, key: key?, name: sanitize_name(&name.unwrap_or_else(|| "Компьютер".into())) })
}

fn write_friends(path: &Path, friends: &[Friend]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut text = String::new();
    for f in friends {
        text.push_str(&format!(
            "v1 {} {} {} {} {}\n",
            hex(&f.id),
            hex(&f.pair),
            hex(&f.their_key),
            if f.renamed { "1" } else { "0" },
            enc_name(&f.name)
        ));
    }
    std::fs::write(path, text)
}

fn read_friends(path: &Path) -> Vec<Friend> {
    let Ok(t) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out = Vec::new();
    for l in t.lines() {
        let mut p = l.split_whitespace();
        if p.next() != Some("v1") {
            continue;
        }
        let Some(id) = p.next().and_then(unhex16) else { continue };
        let Some(pair) = p.next().and_then(unhex32) else { continue };
        let Some(their_key) = p.next().and_then(unhex32) else { continue };
        let renamed = p.next() == Some("1");
        let name = sanitize_name(&dec_name(p.next().unwrap_or("Друг")));
        if out.iter().any(|f: &Friend| f.id == id) {
            continue;
        }
        out.push(Friend { id, their_key, pair, name, renamed });
        if out.len() >= MAX_FRIENDS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Relay(Mutex<Vec<(String, String)>>);
    impl Relay {
        fn post(&self, m: &OutMsg) {
            self.0.lock().unwrap().push((m.topic.clone(), m.body.clone()));
        }
        fn of(&self, topic: &str) -> Vec<String> {
            self.0.lock().unwrap().iter().filter(|(t, _)| t == topic).map(|(_, b)| b.clone()).collect()
        }
        fn dump(&self) -> String {
            self.0.lock().unwrap().iter().map(|(t, m)| format!("{t} {m}\n")).collect()
        }
    }

    fn fixed(n: u8, name: &str) -> Book {
        let mut id = [n; 16];
        id[15] = n.wrapping_mul(3);
        let mut key = [n.wrapping_add(9); 32];
        key[0] = n;
        Book::new(Identity { id, key, name: name.into() })
    }

    #[test]
    fn codes_roundtrip_as_text_and_as_a_link() {
        for n in 1..20 {
            let b = fixed(n, "Даниил");
            let c = b.code();
            assert!(c.contains('-'));
            assert_eq!(parse_code(&c).unwrap().0, b.me.id);
            assert_eq!(parse_code(&c.to_lowercase()).unwrap().1, b.me.key);
            let messy = format!("  {}  ", b.link());
            assert_eq!(parse_code(&messy).unwrap().0, b.me.id);
            assert!(parse_code("ABCD").is_none());
            assert!(parse_code("K7QM-X2PD").is_none());
            assert!(parse_code("").is_none());
        }
    }

    #[test]
    fn two_profiles_add_each_other_then_one_invites() {
        let relay = Relay::default();
        let mut a = fixed(1, "Хост");
        let mut b = fixed(2, "Зритель");
        // B pastes A's code once.
        let hello = b.add(&a.link()).unwrap();
        assert_eq!(b.friends.len(), 1);
        // The hello lands on A's inbox. That name is a hash of id+key, not the id by itself.
        assert_eq!(hello.topic, a.inbox_topic());
        assert_ne!(hello.topic, format!("ssf{}", hex(&a.me.id)));
        assert!(hello.topic.starts_with("ssf") && hello.topic.len() <= 64);
        relay.post(&hello);
        assert_eq!(a.take_hellos(&relay.of(&a.inbox_topic())), 1);
        assert_eq!(a.friends[0].name, "Зритель");
        assert_eq!(a.friends[0].id, b.me.id);
        assert_eq!(a.pair_topic(&b.me.id), b.pair_topic(&a.me.id));
        assert_ne!(a.pair_topic(&b.me.id).unwrap(), a.inbox_topic());

        // Presence: B learns A's name and sees A online.
        let now = 1_700_000_000u64;
        relay.post(&a.presence(&b.me.id, now).unwrap());
        let ev = b.open_pair(&a.me.id, &relay.of(b.pair_topic(&a.me.id).unwrap().as_str())[0]);
        assert!(matches!(ev, Some(PairEvent::Presence { .. })));
        assert_eq!(b.friends[0].name, "Хост");
        assert!(is_online(Some(now), now + 30, 1));
        assert!(!is_online(Some(now), now + online_ttl_secs(1) + 5, 1));
        assert!(!is_online(None, now, 1));

        // A invites. B's "Смотреть" is the room code easy.rs already understands.
        let room = "K7QM-X2PD";
        relay.post(&a.invite(&b.me.id, InviteKind::Room, room, now).unwrap());
        let msgs = relay.of(&b.pair_topic(&a.me.id).unwrap());
        let inv = msgs.iter().find_map(|m| match b.open_pair(&a.me.id, m) {
            Some(PairEvent::Invite(i)) => Some(i),
            _ => None,
        }).unwrap();
        assert_eq!(inv.kind, InviteKind::Room);
        assert_eq!(inv.code, room);
        assert_eq!(inv.name, "Хост");
        assert!(invite_fresh(inv.unix, now + 10));
        assert!(!invite_fresh(inv.unix, now + INVITE_TTL_SECS + 1));

        // What the relay stored contains neither code, nor names, nor ids.
        let seen = relay.dump();
        assert!(!seen.contains(room));
        assert!(!seen.contains("Хост") && !seen.contains("Зритель"));
        assert!(!seen.contains(&hex(&a.me.id)) && !seen.contains(&hex(&b.me.key)));
    }

    #[test]
    fn both_paste_the_code_and_still_share_one_channel() {
        let mut a = fixed(21, "A");
        let mut b = fixed(22, "B");
        let to_b = a.add(&b.code()).unwrap();
        let to_a = b.add(&a.code()).unwrap();
        assert_eq!(a.take_hellos(&[to_a.body]), 1);
        assert_eq!(b.take_hellos(&[to_b.body]), 1);
        assert_eq!(a.friends[0].pair, b.friends[0].pair);
        assert_eq!(a.pair_topic(&b.me.id), b.pair_topic(&a.me.id));
        let msg = a.invite(&b.me.id, InviteKind::Offer, "Hcode", 10).unwrap();
        assert!(matches!(b.open_pair(&a.me.id, &msg.body), Some(PairEvent::Invite(_))));
        // Adding again does not fork the channel.
        let again = b.add(&a.code()).unwrap();
        assert_eq!(a.take_hellos(&[again.body]), 1);
        assert_eq!(a.friends[0].pair, b.friends[0].pair);
    }

    #[test]
    fn a_saved_random_pair_is_not_replaced_by_a_readd_or_a_stray_hello() {
        let mut old = fixed(30, "Старый");
        let mut newer = fixed(31, "Новый");
        let random = [42u8; 32];
        // What 0.12.8 wrote down after one of them pasted the code: the same random secret on both sides.
        old.friends.push(Friend { id: newer.me.id, their_key: newer.me.key, pair: random, name: "Новый".into(), renamed: false });
        newer.friends.push(Friend { id: old.me.id, their_key: old.me.key, pair: random, name: "Старый".into(), renamed: false });

        let again = newer.add(&old.code()).unwrap();
        assert_eq!(newer.friends[0].pair, random);
        assert_eq!(old.take_hellos(&[again.body]), 1);
        assert_eq!(old.friends[0].pair, random);
        assert_eq!(old.pair_topic(&newer.me.id), newer.pair_topic(&old.me.id));

        let stray = seal_hello(&newer.me, &old.me.id, &old.me.key, &[7u8; 32]);
        old.take_hellos(&[stray.body]);
        assert_eq!(old.friends[0].pair, random);

        // A hello that carries the derived secret does pull a leftover random pair onto it.
        let canonical = pair_secret(&newer.me.id, &newer.me.key, &old.me.id, &old.me.key);
        let heal = seal_hello(&newer.me, &old.me.id, &old.me.key, &canonical);
        assert_eq!(old.take_hellos(&[heal.body]), 1);
        assert_eq!(old.friends[0].pair, canonical);
    }

    #[test]
    fn offer_answer_roundtrip_and_decline() {
        let mut a = fixed(3, "A");
        let mut b = fixed(4, "B");
        let hello = b.add(&a.code()).unwrap();
        assert_eq!(a.take_hellos(&[hello.body]), 1);
        let now = 50u64;
        let inv_msg = a.invite(&b.me.id, InviteKind::Offer, "Hhost-code", now).unwrap();
        let PairEvent::Invite(inv) = b.open_pair(&a.me.id, &inv_msg.body).unwrap() else { panic!("invite") };
        assert_eq!(inv.code, "Hhost-code");
        let ans = b.answer(&a.me.id, &inv.nonce, "Cviewer-code").unwrap();
        match a.open_pair(&b.me.id, &ans.body) {
            Some(PairEvent::Answer { code, nonce }) => {
                assert_eq!(code, "Cviewer-code");
                assert_eq!(nonce, inv.nonce);
            }
            other => panic!("{other:?}"),
        }
        let no = b.decline(&a.me.id, &inv.nonce).unwrap();
        assert!(matches!(a.open_pair(&b.me.id, &no.body), Some(PairEvent::Decline { .. })));
    }

    #[test]
    fn stranger_cannot_open_or_guess_and_a_spoofed_hello_does_not_rotate_the_secret() {
        let mut a = fixed(5, "A");
        let mut b = fixed(6, "B");
        let hello = b.add(&a.code()).unwrap();
        let hello_body = hello.body.clone();
        a.take_hellos(&[hello.body]);
        let pair_before = a.friends[0].pair;
        let stranger = fixed(7, "Чужой");
        // Wrong inbox: a stranger polling their own topic is on a different name.
        assert_ne!(stranger.inbox_topic(), a.inbox_topic());
        assert_eq!(inbox_channel(&stranger.me.id, &stranger.me.key).open(KIND_HELLO, &hello_body), None);
        // A hello that claims B's id but a different inbox key is ignored.
        let mut evil = stranger;
        evil.me.id = b.me.id;
        let forged = seal_hello(&evil.me, &a.me.id, &a.me.key, &[9u8; 32]);
        assert_eq!(a.take_hellos(&[forged.body]), 0);
        assert_eq!(a.friends[0].pair, pair_before);
        // Tamper and kind replay fail.
        let ch = pair_channel(&a.me.id, &b.me.id, &a.friends[0].pair);
        let m = ch.seal(KIND_PRESENCE, "10\nA");
        assert!(ch.open(KIND_INVITE, &m).is_none());
        let mut bytes = m.into_bytes();
        let i = bytes.len() / 2;
        bytes[i] ^= 1;
        assert!(ch.open_any(&String::from_utf8(bytes).unwrap()).is_none());
    }

    #[test]
    fn renamed_friend_keeps_the_local_name() {
        let mut a = fixed(8, "A");
        let b = fixed(9, "Настоящее");
        let hello = {
            let mut bb = b.clone();
            bb.add(&a.code()).unwrap()
        };
        a.take_hellos(&[hello.body]);
        assert!(a.rename(&b.me.id, "  Братан  "));
        assert_eq!(a.friends[0].name, "Братан");
        // Presence from B must not put the real name back over a local rename.
        let ch = pair_channel(&b.me.id, &a.me.id, &a.friends[0].pair);
        let msg = ch.seal(KIND_PRESENCE, "10\nНастоящее");
        a.open_pair(&b.me.id, &msg);
        assert_eq!(a.friends[0].name, "Братан");
    }

    #[test]
    fn store_roundtrip_and_profile_override() {
        let dir = std::env::temp_dir().join(format!("ss-friends-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut b = fixed(10, "Имя с пробелом");
        b.add(&fixed(11, "X").code()).unwrap();
        let id = b.friends[0].id;
        b.rename(&id, "Кот");
        save_book(&dir, &b);
        let loaded = load_book(&dir);
        assert_eq!(loaded.me, b.me);
        assert_eq!(loaded.friends, b.friends);
        // A second folder is a second install.
        let dir2 = std::env::temp_dir().join(format!("ss-friends-test-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir2);
        std::env::set_var("SCREENSHARE_PROFILE", &dir2);
        let other = load_book(&profile_dir());
        assert_ne!(other.me.id, b.me.id);
        std::env::remove_var("SCREENSHARE_PROFILE");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn own_code_and_garbage_are_refused() {
        let mut a = fixed(12, "A");
        assert_eq!(a.add(&a.code()), Err(AddError::OwnCode));
        assert_eq!(a.add("не код"), Err(AddError::BadCode));
        assert_eq!(a.add("Haaaaaaaa"), Err(AddError::BadCode));
        assert!(a.friends.is_empty());
        assert!(!add_error_text(AddError::OwnCode).is_empty());
    }

    #[test]
    fn relay_lines_and_junk() {
        let body = concat!(
            "{\"id\":\"m1\",\"time\":10,\"event\":\"open\",\"topic\":\"t\"}\n",
            "{\"id\":\"m2\",\"time\":11,\"event\":\"message\",\"topic\":\"t\",\"message\":\"abc_DEF-123\"}\n",
            "{\"id\":\"m3\",\"time\":12,\"event\":\"message\",\"topic\":\"t\",\"message\":\"has space\"}\n",
            "garbage\n",
            "{\"id\":\"m4\",\"time\":13,\"event\":\"message\",\"topic\":\"t\",\"message\":\"zzz\"}\n",
        );
        let lines = parse_relay(body);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].id, "m2");
        assert_eq!(lines[0].time, 11);
        assert_eq!(next_since("all", &lines), "m4");
        assert_eq!(next_since("prev", &[]), "prev");
    }

    #[test]
    fn polling_stays_inside_the_relay_limit() {
        for n in 0..=12 {
            let gap = relay_gap_ms(n);
            assert!(gap >= RELAY_MIN_GAP_MS, "n={n} gap={gap}");
            let (mut t, mut req) = (0u64, 0u64);
            let horizon = 15 * 60 * 1000;
            while t < horizon {
                t += gap;
                req += 1;
            }
            let budget = 60 + t / 5000;
            assert!(req <= budget, "n={n}: {req} requests in {t} ms, budget {budget}");
            // A short list still beats at least once a minute.
            if n <= 4 {
                assert!(cycle_secs(n) <= 60, "n={n} cycle {}", cycle_secs(n));
            }
        }
    }

    #[test]
    fn names_are_one_line() {
        assert_eq!(sanitize_name("  а  б \nв  "), "а б в");
        assert_eq!(sanitize_name("   "), "Друг");
        assert_eq!(sanitize_name(&"я".repeat(80)).chars().count(), NAME_MAX);
    }
}
