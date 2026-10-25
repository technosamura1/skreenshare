//! When may the receiver ask the sender for a keyframe?
//!
//! A keyframe is hundreds of packets. Asking for one every time something is lost (and again every 150 ms while it
//! has not arrived) is a loop that feeds on itself on a lossy link: the keyframe is hit by loss too, so another one
//! is requested, which is hit too... This policy asks at most once per cooldown, backs off exponentially while
//! requests stay unanswered, never asks while a keyframe is already arriving, and the cooldown is also a floor
//! between two requests even after a successful keyframe.

pub struct KeyPolicy {
    base_ms: u64,
    max_ms: u64,
    cooldown_ms: u64,
    last_sent: Option<u64>,
    need: bool,
    /// A request went out and no keyframe has arrived since.
    unanswered: bool,
    pub requests: u32,
}

impl KeyPolicy {
    pub fn new(base_ms: u64, max_ms: u64) -> Self {
        KeyPolicy { base_ms, max_ms, cooldown_ms: base_ms, last_sent: None, need: false, unanswered: false, requests: 0 }
    }

    pub fn standard() -> Self {
        Self::new(500, 4000)
    }

    /// The decoder cannot continue without a keyframe.
    pub fn need(&mut self) {
        self.need = true;
    }

    pub fn needed(&self) -> bool {
        self.need
    }

    /// A keyframe was delivered to the decoder.
    pub fn on_key(&mut self) {
        self.need = false;
        self.unanswered = false;
        self.cooldown_ms = self.base_ms;
    }

    /// True if a KEYREQ should be sent now. `key_in_flight`: a keyframe is being received (don't pile on).
    pub fn poll(&mut self, now_ms: u64, key_in_flight: bool) -> bool {
        if !self.need || key_in_flight {
            return false;
        }
        if let Some(t) = self.last_sent {
            if now_ms.saturating_sub(t) < self.cooldown_ms {
                return false;
            }
            if self.unanswered {
                self.cooldown_ms = (self.cooldown_ms * 2).min(self.max_ms);
            }
        }
        self.unanswered = true;
        self.last_sent = Some(now_ms);
        self.requests += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_request_is_immediate_and_then_rate_limited() {
        let mut k = KeyPolicy::standard();
        assert!(!k.poll(0, false));
        k.need();
        assert!(k.poll(0, false));
        for t in 1..500 {
            assert!(!k.poll(t, false));
        }
    }

    #[test]
    fn backs_off_while_unanswered_and_resets_after_a_key() {
        let mut k = KeyPolicy::standard();
        k.need();
        let mut times = vec![];
        for t in 0..20_000 {
            if k.poll(t, false) {
                times.push(t);
            }
        }
        // 0, +500 (wait was 500 -> cooldown doubles after), ...
        let gaps: Vec<u64> = times.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.windows(2).all(|g| g[1] >= g[0]), "{gaps:?}");
        assert!(*gaps.last().unwrap() == 4000, "{gaps:?}");
        assert!(times.len() < 12, "{times:?}");
        k.on_key();
        assert!(!k.needed());
        k.need();
        assert!(k.poll(20_000, false)); // after a successful key a new loss may ask again at once (floor: base cooldown)
        assert!(!k.poll(20_100, false));
        assert!(k.poll(20_500, false));
    }

    #[test]
    fn never_asks_while_a_key_is_arriving() {
        let mut k = KeyPolicy::standard();
        k.need();
        assert!(!k.poll(0, true));
        assert!(!k.poll(10_000, true));
        assert!(k.poll(10_001, false));
    }
}
