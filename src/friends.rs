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
