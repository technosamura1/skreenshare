// Adaptive bitrate on the host: back off when the viewer reports damage retransmissions can't repair,
// creep back up once the link has been clean for a while.
// Pure logic with an injected clock (tools/lossim runs it against a simulated bottleneck link).
use crate::reassembly::Stats;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// What the viewer's last report added since the previous one.
#[derive(Default, Clone, Copy, Debug)]
pub struct ReportDelta {
    pub received: u64,
    pub missed: u64,
    pub residual: u64,
    pub gave_up: u64,
}

impl ReportDelta {
    pub fn total(&self) -> u64 {
        self.received + self.missed
    }
}

/// Viewer -> host report body: running totals, u64 LE each: received | missed | residual | gave_up.
pub fn encode_report(s: &Stats, out: &mut Vec<u8>) {
    for v in [s.received, s.missed, s.residual, s.gave_up] {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Host side: turns running totals into per-interval deltas. Older viewers send only the first two numbers.
#[derive(Default)]
pub struct ReportTracker {
    last: [u64; 4],
}

impl ReportTracker {
    pub fn on_report(&mut self, body: &[u8]) -> Option<ReportDelta> {
        if body.len() < 16 {
            return None;
        }
        let mut now = [0u64; 4];
        for (i, c) in body.chunks_exact(8).take(4).enumerate() {
            now[i] = u64::from_le_bytes(c.try_into().unwrap());
        }
        if now[0] < self.last[0] {
            self.last = [0; 4]; // viewer restarted
        }
        let d = ReportDelta {
            received: now[0] - self.last[0],
            missed: now[1].saturating_sub(self.last[1]),
            residual: now[2].saturating_sub(self.last[2]),
            gave_up: now[3].saturating_sub(self.last[3]),
        };
        self.last = now;
        (d.total() > 0).then_some(d)
    }
}

/// Share of lost fragments whose repair failed that plain random loss explains away (a lost request or a lost
/// resend each cost one more round; at 5% loss each way it is ~15%).
const REPAIR_FAIL: f64 = 0.4;

pub struct RateControl {
    max: f64,
    pub cur: f64,
    last_down: Option<Instant>,
    last_up: Option<Instant>,
    clean_since: Instant,
    /// How long the link must be clean before the next increase (grows if probing keeps failing).
    hold: Duration,
    /// Last few report intervals (~2 s): single intervals are too noisy to judge a ratio.
    window: VecDeque<ReportDelta>,
}

impl RateControl {
    pub const MIN: f64 = 2.0;
    const HOLD_BASE: Duration = Duration::from_secs(3);
    const HOLD_MAX: Duration = Duration::from_secs(24);

    pub fn new(max: f64, now: Instant) -> Self {
        RateControl { max, cur: max, last_down: None, last_up: None, clean_since: now, hold: Self::HOLD_BASE, window: VecDeque::new() }
    }

    /// Feed one report interval; returns the new target bitrate (Mbit/s) if it changed.
    ///
    /// Plain random loss (even 5%) is repaired by retransmission and is NOT a reason to cut quality.
    /// Cut when repairs fail (`residual`, `gave_up`) or when raw loss is so high that repairs eat the link.
    pub fn update(&mut self, now: Instant, d: ReportDelta) -> Option<f64> {
        self.window.push_back(d);
        if self.window.len() > 4 {
            self.window.pop_front();
        }
        let (mut missed, mut residual, mut all) = (0u64, 0u64, 0u64);
        for w in &self.window {
            missed += w.missed;
            residual += w.residual;
            all += w.total();
        }
        let total = all.max(1) as f64;
        let raw = missed as f64 / total;
        let res = residual as f64 / total;
        let before = self.cur;
        let factor = if d.gave_up > 0 || res > 0.02 || raw > 0.25 {
            Some(0.7)
        } else if (res > 0.004 && res > REPAIR_FAIL * raw) || raw > 0.12 {
            // Repairs fail much more often than plain random loss would explain (a lost request or a lost
            // resend is ~2x the loss rate): the path is congested, not just lossy.
            Some(0.85)
        } else {
            None
        };
        if let Some(f) = factor {
            if self.last_down.map_or(true, |t| now.duration_since(t) > Duration::from_millis(1500)) {
                // Probing failed soon after an increase: wait longer before trying again.
                if self.last_up.is_some_and(|t| now.duration_since(t) < Duration::from_secs(10)) {
                    self.hold = (self.hold * 2).min(Self::HOLD_MAX);
                }
                self.cur = (self.cur * f).max(Self::MIN);
                self.last_down = Some(now);
            }
            self.clean_since = now;
        } else if res > 0.0 && res > REPAIR_FAIL * raw || raw > 0.08 {
            self.clean_since = now; // not clean enough to raise, not bad enough to cut
        } else if now.duration_since(self.clean_since) >= self.hold {
            self.cur = (self.cur * 1.08).max(self.cur + 0.3).min(self.max);
            self.clean_since = now;
            self.last_up = Some(now);
        } else if self.last_down.map_or(true, |t| now.duration_since(t) > Duration::from_secs(40)) {
            self.hold = Self::HOLD_BASE;
        }
        ((self.cur - before).abs() > 1e-9).then_some(self.cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(received: u64, missed: u64, residual: u64, gave_up: u64) -> ReportDelta {
        ReportDelta { received, missed, residual, gave_up }
    }

    #[test]
    fn random_loss_alone_keeps_bitrate() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        for i in 1..=60 {
            // 5% raw loss, every loss repaired on the first try
            assert_eq!(rc.update(t + Duration::from_millis(500 * i), d(950, 50, 0, 0)), None);
        }
        assert_eq!(rc.cur, 30.0);
    }

    #[test]
    fn repairs_failing_at_the_rate_random_loss_explains_keep_bitrate() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        for i in 1..=60 {
            // 5% loss each way: ~10% of the lost fragments need a second request
            assert_eq!(rc.update(t + Duration::from_millis(500 * i), d(950, 50, 5, 0)), None);
        }
        assert_eq!(rc.cur, 30.0);
    }

    #[test]
    fn residual_loss_cuts_then_clean_link_raises_slowly() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        let mut now = t;
        let mut tick = |rc: &mut RateControl, dd: ReportDelta| {
            now += Duration::from_millis(500);
            rc.update(now, dd)
        };
        assert!(tick(&mut rc, d(900, 100, 40, 0)).is_some());
        let low = rc.cur;
        assert!(low < 30.0 * 0.75);
        for _ in 0..20 {
            tick(&mut rc, d(1000, 0, 0, 0));
        }
        assert!(rc.cur > low && rc.cur < 30.0, "rises gradually, got {}", rc.cur);
        for _ in 0..600 {
            tick(&mut rc, d(1000, 0, 0, 0));
        }
        assert_eq!(rc.cur, 30.0);
    }

    #[test]
    fn never_below_minimum() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        for i in 1..=100 {
            rc.update(t + Duration::from_secs(2 * i), d(500, 500, 400, 3));
        }
        assert_eq!(rc.cur, RateControl::MIN);
    }
}
