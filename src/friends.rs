//! Friends, the Parsec way, on top of the same public ntfy relay as `easy.rs` — no server of ours.
//!
//! Each install has a random id and inbox key (created once, kept next to gui.ini). The friend code / link
//! is that pair: it names an unguessable inbox topic and is the only key that opens it. Adding someone posts
//! one sealed hello there; both sides then share a pair topic derived from the two ids and a fresh pair
//! secret, so a stranger who never saw the hello cannot find the topic or read it.
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
    pub fn new(me: Identity) -> Book {
        Book { me, friends: Vec::new() }
    }

    pub fn generate(name: &str) -> Book {
        Book::new(Identity::generate(name))
    }

    pub fn code(&self) -> String {
        encode_code(&self.me.id, &self.me.key)
    }

    pub fn link(&self) -> String {
        format!("screenshare://friend/{}", encode_code_raw(&self.me.id, &self.me.key))
    }

    pub fn set_my_name(&mut self, name: &str) {
        self.me.name = sanitize_name(name);
    }

    /// Remember `code`'s owner and seal a hello into their inbox. Re-adding rotates the pair secret
    /// (the other side accepts it because the inbox key matches) but keeps a name the user edited.
    pub fn add(&mut self, code: &str) -> Result<OutMsg, AddError> {
        let (id, key) = parse_code(code).ok_or(AddError::BadCode)?;
        if id == self.me.id {
            return Err(AddError::OwnCode);
        }
        if self.friends.len() >= MAX_FRIENDS && self.friends.iter().all(|f| f.id != id) {
            return Err(AddError::Full);
        }
        let mut pair = [0u8; 32];
        getrandom::fill(&mut pair).expect("rng");
        if let Some(f) = self.friends.iter_mut().find(|f| f.id == id) {
            if f.their_key != key {
                return Err(AddError::BadCode);
            }
            f.pair = pair;
        } else {
            self.friends.push(Friend {
                id,
                their_key: key,
                pair,
                name: "Друг".into(),
                renamed: false,
            });
        }
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
                f.pair = pair;
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
