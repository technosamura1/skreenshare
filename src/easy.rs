//! "Простое подключение (beta)": instead of two long codes pasted both ways, the host shows ONE short room
//! code ("K7QM-X2PD"). The two programs then swap the usual codes (offer H…, answer C…) through a public
//! relay (ntfy.sh topic) — but only as sealed messages:
//!
//!   * room code (40 bits) --PBKDF2-HMAC-SHA256 (150 000 rounds)--> 64 bytes = topic name (16) + AEAD key (32);
//!   * the relay (and anyone watching it) sees a random-looking topic and ChaCha20-Poly1305 blobs;
//!   * the sealed text is kind byte ('O' offer / 'A' answer) + the ordinary H…/C… code; the kind is also the AEAD
//!     associated data together with the topic, so a message can't be replayed as the other kind;
//!   * wrong code → different topic: the stranger hears nothing at all. The answer still has to pass the
//!     existing check (session token inside the C… code), and a room lives only while the host's window waits.
//!
//! This file is pure logic (std + crypto crates) so `tools/lossim` can test a whole simulated exchange.
//! The HTTP part lives in `ntfy.rs`.
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use sha2::Sha256;

/// 32 symbols, no 0/O/1/I: easy to read aloud.
const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 8;
const KDF_ROUNDS: u32 = 150_000;
const SALT: &[u8] = b"screenshare-easy-pairing-v1";

pub const KIND_OFFER: u8 = b'O';
pub const KIND_ANSWER: u8 = b'A';

/// A fresh random room code, formatted "ABCD-EFGH".
pub fn new_room_code() -> String {
    let mut b = [0u8; CODE_LEN];
    getrandom::fill(&mut b).expect("rng");
    let s: String = b.iter().map(|x| ALPHABET[(*x & 31) as usize] as char).collect();
    format!("{}-{}", &s[..4], &s[4..])
}

/// What the user typed → canonical 8 symbols, or None if it can't be a room code.
/// Case, spaces and dashes don't matter; 0→O is NOT guessed (O isn't in the alphabet) but 0/1 are rejected clearly.
pub fn normalize(input: &str) -> Option<String> {
    let s: String = input.chars().filter(|c| !c.is_whitespace() && *c != '-' && *c != '_').collect::<String>().to_ascii_uppercase();
    if s.len() == CODE_LEN && s.bytes().all(|b| ALPHABET.contains(&b)) {
        Some(s)
    } else {
        None
    }
}

pub struct Room {
    pub topic: String,
    key: [u8; 32],
}

impl Room {
    pub fn from_code(input: &str) -> Option<Room> {
        let code = normalize(input)?;
        let mut out = [0u8; 48];
        pbkdf2::pbkdf2_hmac::<Sha256>(code.as_bytes(), SALT, KDF_ROUNDS, &mut out);
        let topic = format!("ss{}", hex(&out[..16]));
        let mut key = [0u8; 32];
        key.copy_from_slice(&out[16..]);
        Some(Room { topic, key })
    }

    fn aad(&self, kind: u8) -> Vec<u8> {
        let mut a = self.topic.as_bytes().to_vec();
        a.push(kind);
        a
    }

    /// Seals `code` (an ordinary "H…"/"C…" string) for the relay: base64url(nonce ‖ ciphertext).
    pub fn seal(&self, kind: u8, code: &str) -> String {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("rng");
        let mut plain = vec![kind];
        plain.extend_from_slice(code.as_bytes());
        let c = ChaCha20Poly1305::new_from_slice(&self.key).expect("key");
        let ct = c
            .encrypt(&Nonce::from(nonce), Payload { msg: &plain, aad: &self.aad(kind) })
            .expect("seal");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        b64(&out)
    }

    /// The code inside a relay message, if it is a genuine message of `kind` for this room.
    pub fn open(&self, kind: u8, message: &str) -> Option<String> {
        let raw = unb64(message.trim())?;
        if raw.len() < 12 + 16 + 2 {
            return None;
        }
        let nonce: [u8; 12] = raw[..12].try_into().ok()?;
        let c = ChaCha20Poly1305::new_from_slice(&self.key).ok()?;
        let plain = c
            .decrypt(&Nonce::from(nonce), Payload { msg: &raw[12..], aad: &self.aad(kind) })
            .ok()?;
        if plain.first() != Some(&kind) {
            return None;
        }
        String::from_utf8(plain[1..].to_vec()).ok()
    }

    /// First message of `kind` among everything the relay returned (oldest first), for polling.
    pub fn find<'a>(&self, kind: u8, messages: impl IntoIterator<Item = &'a String>) -> Option<String> {
        // Newest wins: if the host restarted its offer in the same room (can't happen with fresh codes, but
        // a retry of the same post can), the last one is the live one.
        let all: Vec<&String> = messages.into_iter().collect();
        all.iter().rev().find_map(|m| self.open(kind, m))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()
}

/// `{"id":"..","event":"message","message":"<base64url>"}` lines from ntfy's `?poll=1` → just the messages.
/// (Base64url never contains a quote or backslash, so no JSON unescaping is needed; anything odd is skipped.)
pub fn parse_ntfy_lines(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        if !line.contains("\"event\":\"message\"") {
            continue;
        }
        let Some(i) = line.find("\"message\":\"") else { continue };
        let rest = &line[i + 11..];
        let Some(j) = rest.find('"') else { continue };
        let m = &rest[..j];
        if m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            out.push(m.to_string());
        }
    }
    out
}

/// Polling pace against ntfy.sh's limits (a burst of 60 requests, then one per 5 s): quick at first, then calm.
pub fn poll_delay_ms(attempt: u32) -> u64 {
    if attempt < 15 {
        2000
    } else {
        5500
    }
}

/// How long a room waits for the friend.
pub const ROOM_LIFETIME_SECS: u64 = 15 * 60;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A relay in memory: topic → messages in arrival order.
    #[derive(Default)]
    struct FakeRelay(Mutex<Vec<(String, String)>>);
    impl FakeRelay {
        fn post(&self, topic: &str, msg: &str) {
            self.0.lock().unwrap().push((topic.into(), msg.into()));
        }
        fn poll(&self, topic: &str) -> Vec<String> {
            self.0.lock().unwrap().iter().filter(|(t, _)| t == topic).map(|(_, m)| m.clone()).collect()
        }
        fn everything(&self) -> String {
            self.0.lock().unwrap().iter().map(|(t, m)| format!("{t} {m}\n")).collect()
        }
    }

    #[test]
    fn room_codes_look_right_and_parse_forgivingly() {
        for _ in 0..50 {
            let c = new_room_code();
            assert_eq!(c.len(), 9);
            assert_eq!(c.as_bytes()[4], b'-');
            assert!(normalize(&c).is_some());
        }
        assert_eq!(normalize(" abcd efgh ").as_deref(), Some("ABCDEFGH"));
        assert_eq!(normalize("abcd-efgh").as_deref(), Some("ABCDEFGH"));
        assert!(normalize("ABCD-EFG").is_none());
        assert!(normalize("ABCD-EFG0").is_none()); // 0 is not in the alphabet
        assert!(normalize("ABCD-EFGHJ").is_none());
    }

    #[test]
    fn full_exchange_through_a_relay() {
        let relay = FakeRelay::default();
        let host_code = "Hq1AAAAAAAAAAAA";
        let answer = "Cq2BBBBBBBBBBBB";
        let room = new_room_code();
        let host = Room::from_code(&room).unwrap();
        let friend = Room::from_code(&room.to_lowercase().replace('-', " ")).unwrap(); // typed sloppily
        assert_eq!(host.topic, friend.topic);

        relay.post(&host.topic, &host.seal(KIND_OFFER, host_code));
        // the host polling for the answer sees only its own offer: nothing yet
        assert_eq!(host.find(KIND_ANSWER, relay.poll(&host.topic).iter()), None);
        // the friend gets the offer, answers
        let got = friend.find(KIND_OFFER, relay.poll(&friend.topic).iter()).unwrap();
        assert_eq!(got, host_code);
        relay.post(&friend.topic, &friend.seal(KIND_ANSWER, answer));
        assert_eq!(host.find(KIND_ANSWER, relay.poll(&host.topic).iter()).as_deref(), Some(answer));

        // what the relay stored contains no codes, no IPs, no room code
        let seen = relay.everything();
        assert!(!seen.contains(host_code) && !seen.contains(answer));
        assert!(!seen.contains(&normalize(&room).unwrap()));
    }

    #[test]
    fn stranger_with_a_wrong_code_gets_nothing() {
        let relay = FakeRelay::default();
        let host = Room::from_code("AAAA-BBBB").unwrap();
        relay.post(&host.topic, &host.seal(KIND_OFFER, "Hsecret"));
        let stranger = Room::from_code("AAAA-BBBC").unwrap();
        assert_ne!(stranger.topic, host.topic);
        assert!(stranger.find(KIND_OFFER, relay.poll(&stranger.topic).iter()).is_none());
        // even handed the host's ciphertext directly, the wrong key can't open it
        let all = relay.poll(&host.topic);
        assert!(stranger.find(KIND_OFFER, all.iter()).is_none());
    }

    #[test]
    fn tampering_and_replaying_as_another_kind_fail() {
        let r = Room::from_code("AAAA-BBBB").unwrap();
        let m = r.seal(KIND_OFFER, "Hcode");
        assert_eq!(r.open(KIND_OFFER, &m).as_deref(), Some("Hcode"));
        assert!(r.open(KIND_ANSWER, &m).is_none(), "an offer must not pass as an answer");
        let mut bytes = m.clone().into_bytes();
        let i = bytes.len() / 2;
        bytes[i] = if bytes[i] == b'A' { b'B' } else { b'A' };
        assert!(r.open(KIND_OFFER, &String::from_utf8(bytes).unwrap()).is_none());
        assert!(r.open(KIND_OFFER, "").is_none());
        assert!(r.open(KIND_OFFER, "!!!not base64!!!").is_none());
        assert!(r.open(KIND_OFFER, "AAAA").is_none());
        // same room, other topic's ciphertext: the topic is part of the AAD
        let other = Room { topic: "ssother".into(), key: r.key };
        assert!(other.open(KIND_OFFER, &m).is_none());
    }

    #[test]
    fn junk_on_the_topic_is_ignored_and_newest_valid_wins() {
        let r = Room::from_code("AAAA-BBBB").unwrap();
        let msgs = vec![
            "spam".to_string(),
            r.seal(KIND_OFFER, "Hold"),
            "x".repeat(300),
            r.seal(KIND_OFFER, "Hnew"),
            "????".to_string(),
        ];
        assert_eq!(r.find(KIND_OFFER, msgs.iter()).as_deref(), Some("Hnew"));
    }

    #[test]
    fn ntfy_lines_are_parsed_and_noise_skipped() {
        let body = concat!(
            "{\"id\":\"a1\",\"time\":1,\"event\":\"open\",\"topic\":\"t\"}\n",
            "{\"id\":\"a2\",\"time\":2,\"event\":\"message\",\"topic\":\"t\",\"message\":\"abc_DEF-123\"}\n",
            "{\"id\":\"a3\",\"time\":3,\"event\":\"message\",\"topic\":\"t\",\"message\":\"has space\"}\n",
            "garbage\n",
            "{\"id\":\"a4\",\"time\":4,\"event\":\"keepalive\"}\n",
            "{\"id\":\"a5\",\"time\":5,\"event\":\"message\",\"topic\":\"t\",\"message\":\"zzz\"}\n",
        );
        assert_eq!(parse_ntfy_lines(body), vec!["abc_DEF-123".to_string(), "zzz".to_string()]);
    }

    #[test]
    fn polling_stays_inside_the_relay_limit() {
        // total requests in a 15 minute room stay within burst(60) + refill(1/5s)
        let (mut t, mut n, mut a) = (0u64, 0u64, 0u32);
        while t < ROOM_LIFETIME_SECS * 1000 {
            t += poll_delay_ms(a);
            a += 1;
            n += 1;
        }
        assert!(n <= 60 + t / 5000, "{n} requests in {t} ms");
    }

    /// Real network round trip through ntfy.sh with the same curl calls as ntfy.rs (run by hand:
    /// `cargo test --release -- --ignored real_ntfy`).
    #[test]
    #[ignore]
    fn real_ntfy_roundtrip() {
        use std::process::Command;
        let run = |args: &[&str]| {
            let o = Command::new("curl").args(["-sS", "-f", "-m", "20"]).args(args).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let room = new_room_code();
        let host = Room::from_code(&room).unwrap();
        let friend = Room::from_code(&room).unwrap();
        let url = format!("https://ntfy.sh/{}", host.topic);
        run(&["--data-raw", &host.seal(KIND_OFFER, "Hreal-offer"), &url]);
        let mut got = None;
        for _ in 0..5 {
            let msgs = parse_ntfy_lines(&run(&[&format!("{url}/json?poll=1&since=all")]));
            got = friend.find(KIND_OFFER, msgs.iter());
            if got.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        assert_eq!(got.as_deref(), Some("Hreal-offer"));
        run(&["--data-raw", &friend.seal(KIND_ANSWER, "Creal-answer"), &url]);
        // (ntfy.sh's cache lags the POST by up to a second or two: a poll right after it can come back empty,
        // which is why the real code polls repeatedly.)
        let mut msgs = Vec::new();
        for _ in 0..6 {
            msgs = parse_ntfy_lines(&run(&[&format!("{url}/json?poll=1&since=all")]));
            if host.find(KIND_ANSWER, msgs.iter()).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        assert_eq!(host.find(KIND_ANSWER, msgs.iter()).as_deref(), Some("Creal-answer"));
        assert_eq!(host.find(KIND_OFFER, msgs.iter()).as_deref(), Some("Hreal-offer"));
    }
}
