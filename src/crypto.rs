//! ChaCha20-Poly1305 for the media packets of one session.
//!
//! The long-term secret is the friend pair. The key actually used is derived from that secret and the
//! session token, so the next connection does not reuse nonces. A packet's nonce is its frame (or audio
//! sequence) plus the stream, never a constant: the same nonce with two different plaintexts would leak
//! the keystream.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use sha2::{Digest, Sha256};

pub fn media_key(pair: &[u8; 32], token: u32) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"screenshare-media-v1");
    h.update(pair);
    h.update(token.to_le_bytes());
    let d = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

/// Bind the long-lived friend key to the fresh 256-bit secret in this host code.
/// A public 32-bit routing token alone repeats after only about 2^16 sessions.
pub fn friend_session_key(pair: &[u8; 32], session: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"screenshare-friend-media-v2");
    h.update(pair);
    h.update(session);
    h.finalize().into()
}

#[derive(Default)]
pub struct ReplayWindow {
    highest: Option<u64>,
    seen: u128,
}
impl ReplayWindow {
    /// Called only after authentication. Allow reordered packets, never a duplicate.
    pub fn accept(&mut self, n: u64) -> bool {
        let Some(high) = self.highest else {
            self.highest = Some(n);
            self.seen = 1;
            return true;
        };
        if n > high {
            let delta = n - high;
            self.seen = if delta >= 128 {
                1
            } else {
                (self.seen << delta) | 1
            };
            self.highest = Some(n);
            true
        } else {
            let delta = high - n;
            if delta >= 128 || self.seen & (1u128 << delta) != 0 {
                return false;
            }
            self.seen |= 1u128 << delta;
            true
        }
    }
}

pub fn make_nonce(frame_id: u32, frag_idx: u16, stream: u16) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0..4].copy_from_slice(&frame_id.to_le_bytes());
    nonce[4..6].copy_from_slice(&frag_idx.to_le_bytes());
    nonce[6..8].copy_from_slice(&stream.to_le_bytes());
    nonce
}

/// Encrypt `payload`. The clear `header` (already carrying the encrypted packet type) and the nonce are
/// authenticated, so a flipped type or fragment number does not decrypt.
pub fn encrypt_payload(
    key: &[u8; 32],
    nonce: &[u8; 12],
    header: &[u8],
    payload: &[u8],
) -> Option<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).ok()?;
    let mut aad = Vec::with_capacity(header.len() + nonce.len());
    aad.extend_from_slice(header);
    aad.extend_from_slice(nonce);
    cipher
        .encrypt(
            &Nonce::from(*nonce),
            Payload {
                msg: payload,
                aad: &aad,
            },
        )
        .ok()
}

pub fn decrypt_payload(
    key: &[u8; 32],
    nonce: &[u8; 12],
    header: &[u8],
    ciphertext: &[u8],
) -> Option<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).ok()?;
    let mut aad = Vec::with_capacity(header.len() + nonce.len());
    aad.extend_from_slice(header);
    aad.extend_from_slice(nonce);
    cipher
        .decrypt(
            &Nonce::from(*nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .ok()
}

/// `pkt` is a plaintext datagram. `header_len` bytes stay clear (with `enc_type` written over the type),
/// the rest is encrypted. `frame_id` + `frag_idx` + `stream` is the nonce and must be unique for `key`.
pub fn seal(
    key: &[u8; 32],
    pkt: &[u8],
    enc_type: u8,
    header_len: usize,
    frame_id: u32,
    frag_idx: u16,
    stream: u16,
) -> Option<Vec<u8>> {
    if pkt.len() < header_len || header_len == 0 {
        return None;
    }
    let nonce = make_nonce(frame_id, frag_idx, stream);
    let mut header = pkt[..header_len].to_vec();
    header[0] = enc_type;
    let ct = encrypt_payload(key, &nonce, &header, &pkt[header_len..])?;
    let mut out = header;
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Some(out)
}

/// Inverse of [`seal`]. `plain_type` is written back over the ciphertext's type byte.
pub fn open(key: &[u8; 32], pkt: &[u8], plain_type: u8, header_len: usize) -> Option<Vec<u8>> {
    if pkt.len() < header_len + 12 + 16 {
        return None;
    }
    let nonce: [u8; 12] = pkt[header_len..header_len + 12].try_into().ok()?;
    let mut header = pkt[..header_len].to_vec();
    let ct = &pkt[header_len + 12..];
    let body = decrypt_payload(key, &nonce, &header, ct)?;
    header[0] = plain_type;
    header.extend_from_slice(&body);
    Some(header)
}

pub fn parse_key(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

pub fn hex_key(key: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in key {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_video_and_audio_keep_their_headers() {
        let key = media_key(&[7u8; 32], 0x1111_2222);
        let mut video = vec![0x90, 1, 0, 0, 0, 10, 0, 0, 0, 3, 0, 8, 0, 1];
        video.extend_from_slice(b"frame-bytes");
        let sealed = seal(&key, &video, 0x94, 14, 10, 3, 0).unwrap();
        assert_eq!(sealed[0], 0x94);
        assert!(sealed.len() == video.len() + 12 + 16);
        assert_eq!(open(&key, &sealed, 0x90, 14).unwrap(), video);

        let mut audio = vec![0x93, 1, 0, 0, 0, 4, 0, 0, 0];
        audio.extend_from_slice(b"opus");
        let sealed = seal(&key, &audio, 0x95, 5, 4, 0, 1).unwrap();
        assert_eq!(open(&key, &sealed, 0x93, 5).unwrap(), audio);
        // Two audio datagrams must not share a nonce.
        let mut audio2 = audio.clone();
        audio2[5] = 5;
        let other = seal(&key, &audio2, 0x95, 5, 5, 0, 1).unwrap();
        assert_ne!(&sealed[5..], &other[5..]);
        // A retransmission is the same plaintext: the ciphertext must come out identical, not a second nonce.
        assert_eq!(seal(&key, &audio, 0x95, 5, 4, 0, 1).unwrap(), sealed);
    }

    #[test]
    fn wrong_key_tamper_and_reused_session_are_refused() {
        let pair = [9u8; 32];
        let key = media_key(&pair, 1);
        let pkt = b"\x90\x01\x00\x00\x00\x01\x00\x00\x00\x00\x00\x01\x00\x00hello";
        let sealed = seal(&key, pkt, 0x94, 14, 1, 0, 0).unwrap();
        assert!(open(&media_key(&pair, 2), &sealed, 0x90, 14).is_none());
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(open(&key, &bad, 0x90, 14).is_none());
        let mut flipped = sealed.clone();
        flipped[5] ^= 1; // frame id in the clear header, covered by the tag
        assert!(open(&key, &flipped, 0x90, 14).is_none());
    }

    #[test]
    fn friend_keys_change_even_when_the_public_token_repeats() {
        let pair = [7; 32];
        let a = friend_session_key(&pair, &media_key(&[1; 32], 123));
        let b = friend_session_key(&pair, &media_key(&[2; 32], 123));
        assert_ne!(a, b);
    }

    #[test]
    fn replay_window_accepts_reordering_but_not_replays() {
        let mut w = ReplayWindow::default();
        for n in [10, 12, 11, 9, 140, 139] {
            assert!(w.accept(n));
        }
        for n in [10, 12, 11, 9, 140, 139, 0] {
            assert!(!w.accept(n));
        }
        assert!(w.accept(u64::MAX));
        assert!(!w.accept(u64::MAX));
        assert!(w.accept(u64::MAX - 1));
    }

    #[test]
    fn key_hex_roundtrip() {
        let k = [0xab; 32];
        assert_eq!(parse_key(&hex_key(&k)), Some(k));
        assert!(parse_key("zz").is_none());
        assert!(parse_key(&"ab".repeat(31)).is_none());
    }
}
