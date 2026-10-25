//! Simulated packet loss for testing (`SS_SIM_LOSS` on the sender) and the deterministic random numbers the
//! loopback simulation uses. Off by default.

/// xorshift64*: tiny, deterministic, good enough for loss patterns.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Drops a fixed share of the packets it is asked about.
pub struct LossSim {
    p: f64,
    rng: Rng,
    pub seen: u64,
    pub dropped: u64,
}

impl LossSim {
    pub fn new(p: f64, seed: u64) -> Self {
        LossSim { p: p.clamp(0.0, 0.95), rng: Rng::new(seed), seen: 0, dropped: 0 }
    }

    /// Value of the `SS_SIM_LOSS` environment variable: a fraction ("0.05") or a percentage ("5%").
    /// Missing, empty, zero or garbage = off.
    pub fn parse(v: Option<&str>) -> f64 {
        let Some(v) = v.map(str::trim) else { return 0.0 };
        let (num, scale) = match v.strip_suffix('%') {
            Some(n) => (n.trim(), 0.01),
            None => (v, 1.0),
        };
        match num.replace(',', ".").parse::<f64>() {
            Ok(x) if x.is_finite() && x > 0.0 => (x * scale).clamp(0.0, 0.95),
            _ => 0.0,
        }
    }

    pub fn from_env(seed: u64) -> Self {
        Self::new(Self::parse(std::env::var("SS_SIM_LOSS").ok().as_deref()), seed)
    }

    pub fn enabled(&self) -> bool {
        self.p > 0.0
    }

    pub fn rate(&self) -> f64 {
        self.p
    }

    /// True = pretend this packet was lost.
    pub fn drop_next(&mut self) -> bool {
        self.seen += 1;
        if self.p > 0.0 && self.rng.f64() < self.p {
            self.dropped += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variants() {
        assert_eq!(LossSim::parse(None), 0.0);
        assert_eq!(LossSim::parse(Some("")), 0.0);
        assert_eq!(LossSim::parse(Some("abc")), 0.0);
        assert_eq!(LossSim::parse(Some("-1")), 0.0);
        assert!((LossSim::parse(Some("0.05")) - 0.05).abs() < 1e-12);
        assert!((LossSim::parse(Some("5%")) - 0.05).abs() < 1e-12);
        assert!((LossSim::parse(Some("0,03")) - 0.03).abs() < 1e-12);
        assert_eq!(LossSim::parse(Some("7")), 0.95); // clamped: never "drop everything"
    }

    #[test]
    fn off_by_default_and_close_to_the_requested_share() {
        let mut off = LossSim::new(0.0, 1);
        assert!(!off.enabled());
        assert!((0..10_000).all(|_| !off.drop_next()));
        let mut s = LossSim::new(0.05, 42);
        let n = (0..200_000).filter(|_| s.drop_next()).count();
        assert!((9_000..11_000).contains(&n), "{n}");
    }
}
