//! Adaptive bitrate for a weak (2.4 GHz) Wi-Fi link.
//!
//! Called once a second with what the receiver reported. Lowers the bitrate fast when frames get lost or the
//! round trip time balloons, and raises it slowly (and not above a level that just failed) when things are clean.

/// What the sender knows after one second.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// Frames the receiver could not assemble (lost packets) during the last second.
    pub lost_frames: u32,
    /// Frames the receiver threw away (decoder behind / too late) during the last second.
    pub dropped_frames: u32,
    /// Round trip time of the control PING, ms (0 = unknown).
    pub rtt_ms: f64,
}

pub struct RateController {
    cur: f64,
    min: f64,
    max: f64,
    /// Not above this until a long calm period (the level that last failed, minus a margin).
    soft_max: f64,
    clean: u32,
    calm: u32,
    backoff: u32,
    rtt_floor: f64,
    applied: f64,
}

const BACKOFF_MIN_S: u32 = 8;
const BACKOFF_MAX_S: u32 = 30;
const CALM_RESET_S: u32 = 60;

impl RateController {
    pub fn new(start_mbps: f64, min_mbps: f64, max_mbps: f64) -> Self {
        let max = max_mbps.max(min_mbps);
        let cur = start_mbps.clamp(min_mbps, max);
        RateController {
            cur,
            min: min_mbps,
            max,
            soft_max: max,
            clean: 0,
            calm: 0,
            backoff: BACKOFF_MIN_S,
            rtt_floor: f64::MAX,
            applied: cur,
        }
    }

    pub fn mbps(&self) -> f64 {
        self.cur
    }

    /// Feeds one second of feedback. Returns the new bitrate (Mbps) if it should be applied to the encoder.
    pub fn on_sample(&mut self, s: Sample) -> Option<f64> {
        let mut rtt_bad = false;
        if s.rtt_ms > 0.0 {
            self.rtt_floor = if self.rtt_floor == f64::MAX { s.rtt_ms } else { (self.rtt_floor + 0.05).min(s.rtt_ms) };
            rtt_bad = s.rtt_ms > self.rtt_floor + 50.0;
        }
        let heavy = s.lost_frames >= 3 || rtt_bad && s.lost_frames > 0;
        let bad = s.lost_frames > 0 || s.dropped_frames >= 3 || rtt_bad;
        if bad {
            let factor = if heavy { 0.65 } else { 0.85 };
            self.soft_max = (self.cur * 0.92).max(self.min);
            if self.clean < self.backoff.saturating_mul(2) {
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX_S); // failing again soon after a raise: wait longer
            }
            self.cur = (self.cur * factor).max(self.min);
            self.clean = 0;
            self.calm = 0;
        } else {
            self.clean += 1;
            self.calm += 1;
            if self.calm >= CALM_RESET_S {
                self.soft_max = self.max;
                self.backoff = BACKOFF_MIN_S;
            }
            let cap = self.soft_max.min(self.max);
            if self.clean >= self.backoff && self.cur < cap {
                self.cur = (self.cur + (self.cur * 0.1).max(1.0)).min(cap);
                self.clean = 0;
            }
        }
        if (self.cur - self.applied).abs() >= 0.5 || (self.cur <= self.min && self.applied > self.min) {
            self.applied = self.cur;
            Some(self.cur)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: Sample = Sample { lost_frames: 0, dropped_frames: 0, rtt_ms: 5.0 };

    #[test]
    fn loss_lowers_quickly_down_to_the_floor() {
        let mut r = RateController::new(20.0, 6.0, 40.0);
        let n = r.on_sample(Sample { lost_frames: 5, ..OK }).unwrap();
        assert!((n - 13.0).abs() < 0.01, "{n}");
        for _ in 0..10 {
            r.on_sample(Sample { lost_frames: 5, ..OK });
        }
        assert_eq!(r.mbps(), 6.0);
    }

    #[test]
    fn mild_trouble_is_a_small_step() {
        let mut r = RateController::new(20.0, 6.0, 40.0);
        let n = r.on_sample(Sample { lost_frames: 1, ..OK }).unwrap();
        assert!((n - 17.0).abs() < 0.01, "{n}");
    }

    #[test]
    fn rtt_spike_counts_as_congestion() {
        let mut r = RateController::new(20.0, 6.0, 40.0);
        for _ in 0..5 {
            assert!(r.on_sample(OK).is_none());
        }
        let n = r.on_sample(Sample { rtt_ms: 120.0, ..OK }).unwrap();
        assert!(n < 20.0);
    }

    #[test]
    fn raises_slowly_and_not_above_what_just_failed() {
        let mut r = RateController::new(20.0, 6.0, 40.0);
        r.on_sample(Sample { lost_frames: 5, ..OK }); // 13, soft cap 18.4
        let mut hist = vec![];
        for _ in 0..200 {
            if let Some(n) = r.on_sample(OK) {
                hist.push(n);
            }
        }
        assert!(!hist.is_empty());
        // first raise only after the back-off, and no jump
        assert!(hist[0] > 13.0 && hist[0] < 15.5, "{hist:?}");
        // the capped level is respected until a long calm period, then it keeps climbing towards the ceiling
        assert!(hist.iter().cloned().fold(0.0, f64::max) <= 40.0 + 1e-9);
        assert!(r.mbps() > 18.4, "{}", r.mbps());
    }

    #[test]
    fn never_exceeds_ceiling_or_goes_below_floor() {
        let mut r = RateController::new(20.0, 6.0, 25.0);
        for _ in 0..1000 {
            r.on_sample(OK);
        }
        assert_eq!(r.mbps(), 25.0);
        let mut r = RateController::new(3.0, 6.0, 25.0);
        assert_eq!(r.mbps(), 6.0);
        assert!(r.on_sample(Sample { lost_frames: 9, ..OK }).is_none());
    }

    #[test]
    fn no_oscillation_storm_on_a_link_that_fits_16_mbps() {
        let mut r = RateController::new(20.0, 6.0, 40.0);
        let mut changes = 0;
        for _ in 0..300 {
            let lost = if r.mbps() > 16.0 { 2 } else { 0 };
            if r.on_sample(Sample { lost_frames: lost, ..OK }).is_some() {
                changes += 1;
            }
        }
        assert!(changes < 25, "{changes}");
        assert!(r.mbps() >= 10.0);
    }
}
