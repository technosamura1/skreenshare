// Viewer-side performance bookkeeping, pure std (tested on Linux through tools/lossim): accumulators for the
// diagnostics line, an arrival jitter meter, the "hardware decoder isn't really working" detector, the
// convert-cost governor and the catch-up rule.

/// Microsecond samples -> (average ms, max ms, count) per reporting interval.
#[derive(Default, Clone, Copy)]
pub struct Acc {
    n: u64,
    sum: u64,
    max: u64,
}

impl Acc {
    pub fn add(&mut self, us: u64) {
        self.n += 1;
        self.sum += us;
        self.max = self.max.max(us);
    }

    /// Returns (avg ms, max ms, samples) and starts a new interval.
    pub fn take(&mut self) -> (f64, f64, u64) {
        let r = if self.n == 0 { (0.0, 0.0, 0) } else { (self.sum as f64 / self.n as f64 / 1000.0, self.max as f64 / 1000.0, self.n) };
        *self = Acc::default();
        r
    }
}

/// Frames that are this many frames behind the newest one are decoded but not shown (catch-up).
pub const CATCHUP_DEPTH: usize = 3;

/// Whether a frame with `waiting` more frames queued behind it should be converted and shown.
/// A frame is always decoded (P-frames depend on each other); only the expensive part is skipped.
pub fn should_show(waiting: usize) -> bool {
    waiting < CATCHUP_DEPTH
}

/// Arrival jitter of completed frames: smoothed mean absolute deviation of the frame interval, the longest gap
/// and how many gaps over 100 ms followed a flowing stream (a static screen also makes such gaps: no frames are sent).
#[derive(Default)]
pub struct JitterMeter {
    last_us: Option<u64>,
    mean: f64,
    dev: f64,
    prev_dt: f64,
    max_gap: f64,
    stalls: u32,
}

impl JitterMeter {
    pub fn feed(&mut self, now_us: u64) {
        if let Some(l) = self.last_us {
            let dt = now_us.saturating_sub(l) as f64;
            if dt > 100_000.0 && self.prev_dt < 40_000.0 && self.prev_dt > 0.0 {
                self.stalls += 1;
            }
            self.max_gap = self.max_gap.max(dt);
            if dt < 100_000.0 {
                // gaps from an idle screen must not pollute the jitter estimate
                self.mean += (dt - self.mean) / 16.0;
                self.dev += ((dt - self.mean).abs() - self.dev) / 16.0;
            }
            self.prev_dt = dt;
        }
        self.last_us = Some(now_us);
    }

    /// (jitter ms, longest gap ms, stalls) since the last call.
    pub fn take(&mut self) -> (f64, f64, u32) {
        let r = (self.dev / 1000.0, self.max_gap / 1000.0, self.stalls);
        self.max_gap = 0.0;
        self.stalls = 0;
        r
    }
}

/// Notices a hardware decoder that was opened but doesn't deliver: several errors in a row, or a pile of
/// packets and no picture at all.
pub struct HwWatch {
    errors: u32,
    packets: u32,
    pictures: u32,
}

impl HwWatch {
    pub const MAX_ERRORS: u32 = 5;
    pub const MAX_SILENT_PACKETS: u32 = 40;

    pub fn new() -> Self {
        HwWatch { errors: 0, packets: 0, pictures: 0 }
    }

    /// One packet went through the decoder: `ok` = no error, `pictures` = how many frames it produced.
    /// Returns true when the hardware path should be abandoned for the software decoder.
    pub fn on_packet(&mut self, ok: bool, pictures: u32) -> bool {
        self.packets += 1;
        self.pictures += pictures;
        if ok {
            self.errors = 0;
        } else {
            self.errors += 1;
        }
        self.errors >= Self::MAX_ERRORS || (self.pictures == 0 && self.packets >= Self::MAX_SILENT_PACKETS)
    }
}

/// Colour conversion + scaling runs on the CPU; when it gets slow (a 4K stream on a weak PC) the cheaper
/// bilinear filter takes over, and bicubic returns when there is plenty of time again.
pub struct CostGovernor {
    ema_ms: f64,
    pub fast: bool,
}

impl CostGovernor {
    pub const ENTER_MS: f64 = 9.0;
    pub const LEAVE_MS: f64 = 3.5;

    pub fn new() -> Self {
        CostGovernor { ema_ms: 0.0, fast: false }
    }

    pub fn feed(&mut self, ms: f64) -> bool {
        self.ema_ms = if self.ema_ms == 0.0 { ms } else { self.ema_ms + (ms - self.ema_ms) / 8.0 };
        if !self.fast && self.ema_ms > Self::ENTER_MS {
            self.fast = true;
        } else if self.fast && self.ema_ms < Self::LEAVE_MS {
            self.fast = false;
        }
        self.fast
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acc_reports_and_resets() {
        let mut a = Acc::default();
        for us in [1000, 3000, 8000] {
            a.add(us);
        }
        let (avg, max, n) = a.take();
        assert!((avg - 4.0).abs() < 1e-9 && (max - 8.0).abs() < 1e-9 && n == 3);
        assert_eq!(a.take().2, 0);
    }

    #[test]
    fn catch_up_shows_only_near_the_newest() {
        assert!(should_show(0) && should_show(CATCHUP_DEPTH - 1));
        assert!(!should_show(CATCHUP_DEPTH) && !should_show(40));
    }

    #[test]
    fn steady_stream_has_low_jitter_and_no_stalls() {
        let mut j = JitterMeter::default();
        for i in 0..200u64 {
            j.feed(i * 16_667);
        }
        let (jit, gap, stalls) = j.take();
        assert!(jit < 0.5, "{jit}");
        assert!((gap - 16.667).abs() < 0.01 && stalls == 0);
    }

    #[test]
    fn uneven_arrival_raises_jitter_and_a_stall_is_counted_once() {
        let mut j = JitterMeter::default();
        let mut t = 0u64;
        for i in 0..100u64 {
            t += if i % 2 == 0 { 5_000 } else { 28_000 };
            j.feed(t);
        }
        let (jit, _, _) = j.take();
        assert!(jit > 5.0, "{jit}");
        t += 400_000;
        j.feed(t);
        let (_, gap, stalls) = j.take();
        assert!(gap > 399.0 && stalls == 1);
        // a pause right after another pause (idle screen) is not a new stall
        t += 400_000;
        j.feed(t);
        assert_eq!(j.take().2, 0);
    }

    #[test]
    fn hw_watch_gives_up_on_errors_or_silence_but_not_on_one_glitch() {
        let mut w = HwWatch::new();
        assert!(!w.on_packet(false, 0));
        assert!(!w.on_packet(true, 1)); // recovered: counter reset
        for _ in 0..HwWatch::MAX_ERRORS - 1 {
            assert!(!w.on_packet(false, 0));
        }
        assert!(w.on_packet(false, 0));
        let mut w = HwWatch::new();
        for i in 1..=HwWatch::MAX_SILENT_PACKETS {
            assert_eq!(w.on_packet(true, 0), i == HwWatch::MAX_SILENT_PACKETS);
        }
        // pictures are coming: silence never triggers
        let mut w = HwWatch::new();
        w.on_packet(true, 1);
        for _ in 0..100 {
            assert!(!w.on_packet(true, 0));
        }
    }

    #[test]
    fn governor_switches_with_hysteresis() {
        let mut g = CostGovernor::new();
        for _ in 0..30 {
            g.feed(2.0);
        }
        assert!(!g.fast);
        for _ in 0..30 {
            g.feed(14.0);
        }
        assert!(g.fast);
        for _ in 0..10 {
            g.feed(6.0); // between the thresholds: stays fast
        }
        assert!(g.fast);
        for _ in 0..60 {
            g.feed(1.0);
        }
        assert!(!g.fast);
    }
}
