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
    /// The viewer's last round-trip time in microseconds (0 = not reported: an older viewer).
    pub rtt_us: u64,
}

impl ReportDelta {
    pub fn total(&self) -> u64 {
        self.received + self.missed
    }
}

/// Viewer -> host report body: running totals, u64 LE each: received | missed | residual | gave_up.
#[allow(dead_code)]
pub fn encode_report(s: &Stats, out: &mut Vec<u8>) {
    encode_report_rtt(s, 0, out)
}

/// Same plus the viewer's round-trip time (a fifth number; an older host ignores it).
pub fn encode_report_rtt(s: &Stats, rtt_us: u64, out: &mut Vec<u8>) {
    for v in [s.received, s.missed, s.residual, s.gave_up, rtt_us] {
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
            rtt_us: body.get(32..40).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap())),
        };
        self.last = now;
        (d.total() > 0).then_some(d)
    }
}

/// Share of lost fragments whose repair failed that plain random loss explains away (a lost request or a lost
/// resend each cost one more round; at 5% loss each way it is ~15%).
const REPAIR_FAIL: f64 = 0.4;
/// Raw loss that is repaired fine but still hurts (retransmissions, delay, late audio) and is often caused by our own
/// sending rate on a Wi-Fi/ISP link: back off gently (x0.85 per step), but only down to this share of the maximum;
/// below that only failing repairs cut further, so a link that is simply lossy keeps decent quality.
const SOFT_LOSS: f64 = 0.04;
const SOFT_FLOOR: f64 = 0.6;

/// "Авто": the start-of-session ramp. Starts low, steps up while the link is clean and the round-trip time stays put,
/// stops at the first sign of trouble (and backs off one step), after which the ordinary adaptive logic takes over.
#[derive(Clone, Copy, Debug)]
pub struct Ramp {
    last_step: Instant,
    /// Smallest round-trip time seen (ms): the "quiet link" reference.
    base_rtt_ms: f64,
}

/// Where Авто starts (Mbit/s), unless the cap is lower.
pub fn auto_start(max: f64) -> f64 {
    RAMP_START.min(max)
}

const RAMP_START: f64 = 20.0;
const RAMP_STEP: Duration = Duration::from_millis(1500);
const RAMP_FACTOR: f64 = 1.25;
/// Raw loss that is still "near zero" for the ramp.
const RAMP_LOSS: f64 = 0.005;

pub struct RateControl {
    max: f64,
    pub cur: f64,
    /// Some while the Авто ramp is still climbing.
    pub ramp: Option<Ramp>,
    /// Set by the ramp when trouble ended it (Авто settled below the maximum).
    pub ramp_stopped_by_loss: bool,
    last_down: Option<Instant>,
    /// Last cut made by the ordinary logic (not the Авто ramp settling): what the network lamp reports.
    cut_at: Option<Instant>,
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
        RateControl {
            max,
            cur: max,
            ramp: None,
            ramp_stopped_by_loss: false,
            last_down: None,
            cut_at: None,
            last_up: None,
            clean_since: now,
            hold: Self::HOLD_BASE,
            window: VecDeque::new(),
        }
    }

    /// Авто: start at `RAMP_START` (or lower when the cap is lower) and climb to `max`.
    pub fn new_auto(max: f64, now: Instant) -> Self {
        let mut rc = Self::new(max, now);
        rc.cur = RAMP_START.min(max);
        if rc.cur < max {
            rc.ramp = Some(Ramp { last_step: now, base_rtt_ms: f64::INFINITY });
        }
        rc
    }

    /// The most the controller may ask for (the cap of the preset, or of the software encoder).
    pub fn max(&self) -> f64 {
        self.max
    }

    /// Lowers the cap (a software encoder can't produce more); a running ramp then simply ends at it.
    pub fn clamp_max(&mut self, cap: f64) {
        if cap < self.max {
            self.max = cap;
        }
        if self.cur > self.max {
            self.cur = self.max;
        }
        if self.cur >= self.max {
            self.ramp = None;
        }
    }

    /// True when quality was recently cut: the lamp says "снизил качество".
    pub fn recently_cut(&self, now: Instant) -> bool {
        self.cut_at.is_some_and(|t| now.duration_since(t) < Duration::from_secs(30))
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
        if let Some(r) = self.ramp {
            if self.update_ramp(now, d, raw, res, r) {
                return ((self.cur - before).abs() > 1e-9).then_some(self.cur);
            }
        }
        let factor = if d.gave_up > 0 || res > 0.02 || raw > 0.25 {
            Some(0.7)
        } else if (res > 0.004 && res > REPAIR_FAIL * raw) || raw > 0.12 || (raw > SOFT_LOSS && self.cur > self.max * SOFT_FLOOR) {
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
                self.cut_at = Some(now);
            }
            self.clean_since = now;
        } else if res > 0.0 && res > REPAIR_FAIL * raw || raw > 0.03 {
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

impl RateControl {
    /// One report during the Авто ramp. Returns true when the ramp handled the report (still climbing or just
    /// stopped); false when the ordinary logic should look at it (the ramp is over and `self.ramp` is None).
    fn update_ramp(&mut self, now: Instant, d: ReportDelta, raw: f64, res: f64, mut r: Ramp) -> bool {
        let rtt_ms = d.rtt_us as f64 / 1000.0;
        if rtt_ms > 0.0 {
            r.base_rtt_ms = r.base_rtt_ms.min(rtt_ms);
        }
        // Round-trip time stable = not above the quiet value by much (a filling queue shows up here before any loss).
        let rtt_bad = rtt_ms > 0.0 && r.base_rtt_ms.is_finite() && rtt_ms > r.base_rtt_ms * 1.5 + 25.0;
        let trouble = d.gave_up > 0 || res > 0.0 || raw > RAMP_LOSS * 24.0 || rtt_bad;
        if trouble {
            // Step back to the last rate that was fine and stay there; the ordinary logic keeps adapting from here.
            let prev = (self.cur / RAMP_FACTOR).max(RAMP_START.min(self.max) * 0.75).max(Self::MIN);
            self.cur = if self.cur > RAMP_START.min(self.max) { prev.max(RAMP_START.min(self.max)) } else { self.cur };
            self.cur = self.cur.min(self.max);
            self.ramp = None;
            self.ramp_stopped_by_loss = true;
            self.last_down = Some(now);
            self.clean_since = now;
            self.hold = Self::HOLD_MAX / 2; // do not poke at the limit again soon
            self.window.clear();
            return true;
        }
        // Reordering alone shows up as a few % "missed" that is repaired at once; what really counts is damage that retransmission could not repair (residual / gave_up) and the round-trip time.
        if raw <= RAMP_LOSS * 16.0 && now.duration_since(r.last_step) >= RAMP_STEP {
            self.cur = (self.cur * RAMP_FACTOR).min(self.max);
            r.last_step = now;
            if self.cur >= self.max {
                self.ramp = None; // reached the top
                self.clean_since = now;
                return true;
            }
        }
        self.ramp = Some(r);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(received: u64, missed: u64, residual: u64, gave_up: u64) -> ReportDelta {
        ReportDelta { received, missed, residual, gave_up, rtt_us: 0 }
    }

    #[test]
    fn random_loss_alone_keeps_bitrate() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        for i in 1..=60 {
            // 3% raw loss, every loss repaired on the first try
            assert_eq!(rc.update(t + Duration::from_millis(500 * i), d(970, 30, 0, 0)), None);
        }
        assert_eq!(rc.cur, 30.0);
    }

    #[test]
    fn steady_loss_of_5_percent_backs_off_gently_but_not_below_the_floor() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        let mut cuts = 0;
        for i in 1..=200 {
            if rc.update(t + Duration::from_millis(500 * i), d(950, 50, 0, 0)).is_some() {
                cuts += 1;
            }
        }
        assert!(cuts >= 1);
        assert!(rc.cur >= 30.0 * SOFT_FLOOR * 0.85 - 1e-9 && rc.cur < 30.0, "got {}", rc.cur);
    }

    #[test]
    fn three_percent_loss_does_not_cut_but_does_not_raise_either() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        rc.cur = 20.0;
        for i in 1..=100 {
            assert_eq!(rc.update(t + Duration::from_millis(500 * i), d(965, 35, 0, 0)), None);
        }
        assert_eq!(rc.cur, 20.0);
    }

    #[test]
    fn repairs_failing_at_the_rate_random_loss_explains_keep_bitrate() {
        let t = Instant::now();
        let mut rc = RateControl::new(30.0, t);
        for i in 1..=60 {
            // 3% loss each way: ~10% of the lost fragments need a second request
            assert_eq!(rc.update(t + Duration::from_millis(500 * i), d(970, 30, 3, 0)), None);
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

    /// The two presets of the GUI (30 "Стандарт", 60 "Высокое"): loss cuts the rate, a clean link brings it back to
    /// exactly the preset and never above it.
    #[test]
    fn both_presets_cut_on_loss_and_recover_to_their_cap() {
        for max in [30.0_f64, 60.0] {
            let t = Instant::now();
            let mut rc = RateControl::new(max, t);
            assert_eq!(rc.cur, max);
            let mut now = t;
            let mut tick = |rc: &mut RateControl, dd: ReportDelta| {
                now += Duration::from_millis(500);
                rc.update(now, dd)
            };
            assert!(tick(&mut rc, d(900, 100, 40, 0)).is_some(), "loss must lower {max}");
            assert!(rc.cur < max * 0.75, "max {max}: {}", rc.cur);
            for _ in 0..2000 {
                tick(&mut rc, d(1000, 0, 0, 0));
                assert!(rc.cur <= max + 1e-9);
            }
            assert_eq!(rc.cur, max);
            // a long bad spell never goes under the soft floor of the preset by much, and never under MIN
            for _ in 0..300 {
                tick(&mut rc, d(950, 50, 0, 0));
            }
            assert!(rc.cur >= max * SOFT_FLOOR * 0.85 - 1e-9 && rc.cur >= RateControl::MIN, "max {max}: {}", rc.cur);
        }
    }

    fn drive(rc: &mut RateControl, t0: Instant, secs: u32, dd: impl Fn(u32) -> ReportDelta) -> Vec<f64> {
        let mut seen = vec![rc.cur];
        for i in 1..=secs * 2 {
            rc.update(t0 + Duration::from_millis(500 * i as u64), dd(i));
            seen.push(rc.cur);
        }
        seen
    }

    fn with_rtt(mut x: ReportDelta, ms: u64) -> ReportDelta {
        x.rtt_us = ms * 1000;
        x
    }

    #[test]
    fn auto_starts_at_20_and_ramps_to_60_on_a_clean_link() {
        let t = Instant::now();
        let mut rc = RateControl::new_auto(60.0, t);
        assert_eq!(rc.cur, 20.0);
        let seen = drive(&mut rc, t, 30, |_| with_rtt(d(1000, 0, 0, 0), 30));
        assert_eq!(rc.cur, 60.0, "{seen:?}");
        assert!(rc.ramp.is_none() && !rc.ramp_stopped_by_loss);
        // it takes a few seconds, not an instant jump, and never overshoots
        let first60 = seen.iter().position(|&v| v == 60.0).unwrap();
        assert!(first60 >= 8 && first60 <= 24, "reached 60 after {first60} reports");
        assert!(seen.iter().all(|&v| v <= 60.0));
    }

    #[test]
    fn auto_stops_and_backs_off_one_step_when_loss_appears() {
        let t = Instant::now();
        let mut rc = RateControl::new_auto(60.0, t);
        // clean for 4 s, then the link starts losing packets that repairs cannot fix
        let (mut peak, mut at_stop) = (0.0f64, None);
        for i in 1..=40u64 {
            let rep = if i < 8 { with_rtt(d(1000, 0, 0, 0), 30) } else { with_rtt(d(900, 100, 40, 0), 30) };
            let was_ramping = rc.ramp.is_some();
            rc.update(t + Duration::from_millis(500 * i), rep);
            if was_ramping && rc.ramp.is_none() && at_stop.is_none() {
                at_stop = Some(rc.cur);
            }
            if at_stop.is_none() {
                peak = peak.max(rc.cur);
            }
        }
        assert!(rc.ramp_stopped_by_loss);
        let at_stop = at_stop.expect("ramp stopped");
        assert!(peak > 20.0 && peak < 60.0, "peak {peak}");
        assert!(at_stop < peak && at_stop >= 20.0, "peak {peak}, after stop {at_stop}");
    }

    #[test]
    fn auto_stops_when_the_round_trip_time_balloons_even_without_loss() {
        let t = Instant::now();
        let mut rc = RateControl::new_auto(60.0, t);
        let seen = drive(&mut rc, t, 20, |i| with_rtt(d(1000, 0, 0, 0), if i < 10 { 30 } else { 400 }));
        assert!(rc.ramp_stopped_by_loss && rc.cur < 60.0, "{seen:?}");
        // and no more climbing right after
        let at_stop = rc.cur;
        let t2 = t + Duration::from_secs(10);
        for i in 1..=4 {
            rc.update(t2 + Duration::from_millis(500 * i), with_rtt(d(1000, 0, 0, 0), 400));
        }
        assert!(rc.cur <= at_stop + 1e-9);
    }

    #[test]
    fn auto_after_the_ramp_still_cuts_on_loss_and_recovers_slowly() {
        let t = Instant::now();
        let mut rc = RateControl::new_auto(60.0, t);
        drive(&mut rc, t, 30, |_| with_rtt(d(1000, 0, 0, 0), 30));
        assert_eq!(rc.cur, 60.0);
        let t2 = t + Duration::from_secs(30);
        assert!(rc.update(t2 + Duration::from_millis(500), d(700, 300, 100, 0)).is_some());
        assert!(rc.cur < 45.0);
        assert!(rc.recently_cut(t2 + Duration::from_secs(1)));
    }

    #[test]
    fn clamp_max_ends_the_ramp_at_a_software_encoder_cap() {
        let t = Instant::now();
        let mut rc = RateControl::new_auto(60.0, t);
        rc.clamp_max(15.0);
        assert_eq!((rc.cur, rc.max(), rc.ramp.is_none()), (15.0, 15.0, true));
        let mut m = RateControl::new(30.0, t);
        m.clamp_max(100.0);
        assert_eq!((m.cur, m.max()), (30.0, 30.0));
    }

    #[test]
    fn report_carries_the_round_trip_time_and_old_reports_still_parse() {
        let st = Stats { received: 100, missed: 5, recovered: 4, residual: 1, gave_up: 0, requested: 0 };
        let mut b = Vec::new();
        encode_report_rtt(&st, 42_000, &mut b);
        let mut tr = ReportTracker::default();
        let dd = tr.on_report(&b).unwrap();
        assert_eq!((dd.received, dd.missed, dd.residual, dd.rtt_us), (100, 5, 1, 42_000));
        let mut old = Vec::new();
        for v in [200u64, 5, 1, 0] {
            old.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(tr.on_report(&old).unwrap().rtt_us, 0);
    }
}
