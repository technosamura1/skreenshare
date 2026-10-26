// Viewer-side audio playout engine (pure std, so it is tested on Linux through tools/lossim).
//
// The sender's sound clock and ours never run at exactly the same speed (typically 50..500 ppm apart, up to
// ~1000 ppm on cheap cards), packets arrive in bursts, and the friend's sound card may run at 44.1 kHz.
// What was audible as crackling before:
//  - an empty buffer cut the sound to digital silence in the middle of a wave (a click), and the restart was
//    another click; now an underrun fades out over 3 ms, re-buffers, and fades back in;
//  - a full buffer dropped the oldest audio in one go (a click); now it fades out, skips, fades in;
//  - the 40..50 ms buffer was too small for bursty networks; now the buffer target adapts between
//    80 and 120 ms (grows 20 ms per underrun, shrinks 5 ms per 30 quiet seconds);
//  - drift was only chased by a proportional controller; now a PI controller learns the drift (ppm) and
//    keeps the delay steady while changing the playback speed by tiny, smooth amounts;
//  - linear interpolation (aliasing "grit", worst for 48 -> 44.1 kHz) is replaced by an 8-tap Kaiser-windowed
//    sinc resampler (>60 dB clean up to ~10 kHz).
use std::collections::VecDeque;

pub const MIN_TARGET: f64 = 0.080;
pub const START_TARGET: f64 = 0.090;
pub const MAX_TARGET: f64 = 0.120;

/// How much sound to keep buffered (seconds): the trade between latency and gaps on a jittery network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profile {
    pub min: f64,
    pub start: f64,
    pub max: f64,
}

/// «Кино»: the picture waits for the sound (lip sync), so a deep cushion costs nothing but delay.
pub const MOVIE: Profile = Profile { min: MIN_TARGET, start: START_TARGET, max: MAX_TARGET };
/// «Игра»: the picture is shown at once; the sound keeps just enough to ride out network jitter.
pub const GAME: Profile = Profile { min: 0.020, start: 0.025, max: 0.080 };

const TARGET_UP: f64 = 0.020;
const TARGET_DOWN: f64 = 0.005;
/// This long without an underrun lets the target shrink one step.
const CALM_SEC: f64 = 30.0;
/// Above target + this the oldest audio is skipped (with a fade).
const TRIM_OVER: f64 = 0.150;
/// Fade length for starts, stops and skips.
const FADE_SEC: f64 = 0.003;
/// Largest speed correction (0.8%: ~14 cents, only reached while catching up after a burst).
const MAX_FIX: f64 = 0.008;
const MAX_INTEG: f64 = 0.005;
const KP: f64 = 0.08; // per second of level error
const KI: f64 = 0.004; // per second² of level error

/// Smoothed buffer level -> playback speed correction. `integ` converges to the clock drift.
#[derive(Default)]
pub struct RateCtl {
    level: Option<f64>,
    integ: f64,
}

impl RateCtl {
    /// Returns the speed correction (+0.001 = play 0.1% faster). Level and target in seconds.
    pub fn update(&mut self, level: f64, target: f64, dt: f64) -> f64 {
        let a = 1.0 - (-dt / 0.3).exp();
        let l = match self.level {
            None => level,
            Some(p) => p + a * (level - p),
        };
        self.level = Some(l);
        let err = l - target;
        self.integ = (self.integ + KI * err * dt).clamp(-MAX_INTEG, MAX_INTEG);
        (KP * err + self.integ).clamp(-MAX_FIX, MAX_FIX)
    }

    /// Learned clock drift in ppm (+: the sender's clock is faster than ours).
    pub fn drift_ppm(&self) -> f64 {
        self.integ * 1e6
    }

    /// The level history is stale (after a re-buffer).
    pub fn reset_level(&mut self) {
        self.level = None;
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PlayStats {
    /// Times the buffer ran dry in the middle of playback (each one is an audible gap).
    pub underruns: u64,
    /// Times the buffer overflowed and old audio was skipped.
    pub trims: u64,
    /// Output frames that were silence because of an underrun.
    pub starved_frames: u64,
    pub level_ms: f64,
    pub target_ms: f64,
    pub drift_ppm: f64,
    /// Sound card: output rate and the largest callback (frames) seen.
    pub out_rate: u32,
    pub cb_frames: u32,
}

#[derive(PartialEq, Clone, Copy)]
enum State {
    Buffering,
    Playing,
}

pub struct Playout {
    /// Interleaved stereo at `src_rate`; the first HISTORY frames are already played (the kernel needs them).
    q: VecDeque<f32>,
    src_rate: u32,
    state: State,
    /// Fractional position between frame HISTORY and HISTORY+1 of q (frames).
    pos: f64,
    kernel: Vec<f32>,
    kernel_for: (u32, u32),
    last_out: [f32; 2],
    target: f64,
    /// Bounds of `target` («Кино» or «Игра»).
    profile: Profile,
    calm: f64,
    ctl: RateCtl,
    gain: f32,
    gain_dir: f32,
    /// A skip was requested (buffer overflow); done at the bottom of a fade.
    trim: bool,
    started: bool,
    pub stats: PlayStats,
}

impl Default for Playout {
    fn default() -> Self {
        Self::new()
    }
}

const TAPS: usize = 8;
/// History frames kept in front of the current one (the kernel reaches 3 frames back, 4 ahead).
const HISTORY: usize = 3;
const PHASES: usize = 512;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..40 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

/// `PHASES` x `TAPS` Kaiser-windowed sinc kernels (each normalised to unity DC gain). When playing at a lower
/// rate than we receive, the cut-off drops below the source's Nyquist so nothing aliases.
fn make_kernel(src: f64, dst: f64) -> Vec<f32> {
    let fc = (dst / src).min(1.0) * 0.98;
    let beta = 8.0;
    let i0b = bessel_i0(beta);
    let mut k = vec![0f32; PHASES * TAPS];
    for ph in 0..PHASES {
        let t = ph as f64 / PHASES as f64;
        let mut h = [0f64; TAPS];
        let mut sum = 0.0;
        for (i, hi) in h.iter_mut().enumerate() {
            let d = i as f64 - HISTORY as f64 - t;
            let w = if d.abs() < 4.0 { bessel_i0(beta * (1.0 - (d / 4.0).powi(2)).sqrt()) / i0b } else { 0.0 };
            let x = std::f64::consts::PI * fc * d;
            let s = if x.abs() < 1e-9 { 1.0 } else { x.sin() / x };
            *hi = fc * s * w;
            sum += *hi;
        }
        for i in 0..TAPS {
            k[ph * TAPS + i] = (h[i] / sum) as f32;
        }
    }
    k
}

impl Playout {
    pub fn new() -> Self {
        Playout {
            q: VecDeque::from(vec![0.0; HISTORY * 2]),
            src_rate: 48000,
            state: State::Buffering,
            pos: 0.0,
            kernel: Vec::new(),
            kernel_for: (0, 0),
            last_out: [0.0; 2],
            target: START_TARGET,
            profile: MOVIE,
            calm: 0.0,
            ctl: RateCtl::default(),
            gain: 0.0,
            gain_dir: 0.0,
            trim: false,
            started: false,
            stats: PlayStats { target_ms: START_TARGET * 1000.0, ..Default::default() },
        }
    }

    /// Switches between «Кино» and «Игра» on the fly. Going to a smaller cushion skips the surplus at once
    /// (faded out and in, no click) instead of slowly speeding it away.
    pub fn set_profile(&mut self, p: Profile) {
        if self.profile == p {
            return;
        }
        self.profile = p;
        // Start over from the new style's own level (clamping the old target would leave «Игра» at its maximum).
        self.target = p.start;
        self.calm = 0.0;
        self.ctl.reset_level();
        if self.state == State::Playing && self.level_sec() > self.target + 0.030 {
            self.trim = true;
        }
        self.stats.target_ms = self.target * 1000.0;
    }

    /// Interleaved stereo samples from the network.
    pub fn push(&mut self, samples: &[i16], rate: u32) {
        if self.src_rate != rate {
            self.q = VecDeque::from(vec![0.0; HISTORY * 2]);
            self.src_rate = rate;
            self.state = State::Buffering;
            self.pos = 0.0;
            self.ctl = RateCtl::default();
        }
        self.q.extend(samples.iter().map(|&s| s as f32 * (1.0 / 32768.0)));
        if self.level_sec() > self.target + TRIM_OVER {
            self.trim = true;
        }
    }

    pub fn level_sec(&self) -> f64 {
        (self.q.len() / 2).saturating_sub(HISTORY) as f64 / self.src_rate as f64
    }

    /// Fills `out` (interleaved, `ch` channels, `dst_rate` Hz). Never blocks (allocates only when the rates change).
    pub fn render(&mut self, out: &mut [f32], ch: usize, dst_rate: f64) {
        let frames = out.len() / ch.max(1);
        let dt = frames as f64 / dst_rate;
        let fade_step = (1.0 / (FADE_SEC * dst_rate)) as f32;
        let src = self.src_rate as f64;
        if self.kernel_for != (self.src_rate, dst_rate as u32) {
            self.kernel = make_kernel(src, dst_rate);
            self.kernel_for = (self.src_rate, dst_rate as u32);
        }
        self.stats.out_rate = dst_rate as u32;
        self.stats.cb_frames = self.stats.cb_frames.max(frames as u32);
        if self.state == State::Buffering && self.level_sec() >= self.target {
            self.state = State::Playing;
            self.started = true;
            self.gain = 0.0;
            self.gain_dir = 1.0;
            self.ctl.reset_level();
        }
        let mut fix = 0.0;
        if self.state == State::Playing {
            fix = self.ctl.update(self.level_sec(), self.target, dt);
            self.calm += dt;
            if self.calm >= CALM_SEC {
                self.calm = 0.0;
                self.target = (self.target - TARGET_DOWN).max(self.profile.min);
            }
        }
        let step = src / dst_rate * (1.0 + fix);
        for f in out.chunks_mut(ch) {
            // 1. where does this frame's sample come from
            let mut s = self.last_out;
            if self.state == State::Playing {
                if self.q.len() >= TAPS * 2 {
                    let ph = ((self.pos * PHASES as f64) as usize).min(PHASES - 1);
                    let h = &self.kernel[ph * TAPS..(ph + 1) * TAPS];
                    let (mut l, mut r) = (0.0f32, 0.0f32);
                    for (i, w) in h.iter().enumerate() {
                        l += w * self.q[i * 2];
                        r += w * self.q[i * 2 + 1];
                    }
                    s = [l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0)];
                    self.pos += step;
                    while self.pos >= 1.0 && self.q.len() >= 2 {
                        self.q.pop_front();
                        self.q.pop_front();
                        self.pos -= 1.0;
                    }
                    self.last_out = s;
                } else {
                    // Ran dry: fade what we were playing out, wait for the buffer to refill.
                    self.state = State::Buffering;
                    self.gain_dir = -1.0;
                    self.stats.underruns += 1;
                    self.target = (self.target + TARGET_UP).min(self.profile.max);
                    self.calm = 0.0;
                    self.pos = 0.0;
                }
            }
            if self.state == State::Buffering && self.started && self.gain <= 0.0 {
                self.stats.starved_frames += 1;
            }
            // 2. skip ahead on overflow, at the bottom of a fade
            if self.trim && self.state == State::Playing {
                if self.gain_dir >= 0.0 && self.gain >= 1.0 {
                    self.gain_dir = -1.0;
                } else if self.gain <= 0.0 {
                    let keep = (self.target * src) as usize + HISTORY;
                    let have = self.q.len() / 2;
                    if have > keep {
                        self.q.drain(..(have - keep) * 2);
                    }
                    self.pos = 0.0;
                    self.trim = false;
                    self.stats.trims += 1;
                    self.gain_dir = 1.0;
                }
            }
            // 3. fade
            self.gain = (self.gain + self.gain_dir * fade_step).clamp(0.0, 1.0);
            if self.gain >= 1.0 && self.gain_dir > 0.0 || self.gain <= 0.0 && self.gain_dir < 0.0 {
                if self.gain <= 0.0 {
                    self.last_out = [0.0; 2];
                }
                self.gain_dir = 0.0;
            }
            let g = self.gain * self.gain * (3.0 - 2.0 * self.gain);
            if ch == 1 {
                f[0] = (s[0] + s[1]) * 0.5 * g;
            } else {
                f[0] = s[0] * g;
                f[1] = s[1] * g;
                for x in f[2..].iter_mut() {
                    *x = 0.0;
                }
            }
        }
        self.stats.level_ms = self.level_sec() * 1000.0;
        self.stats.target_ms = self.target * 1000.0;
        self.stats.drift_ppm = self.ctl.drift_ppm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAU: f64 = std::f64::consts::TAU;

    /// Simulates a network source (clock `1 + drift` x nominal, packets every `burst_ms`, with jitter) and an
    /// output device with `cb_ms` callbacks; returns (stats, max sample step seen after warm-up, output).
    struct Sim {
        p: Playout,
        src_rate: f64,
        dst_rate: f64,
        t_src: f64,
        frac: f64,
        phase: f64,
        freq: f64,
        out: Vec<f32>,
    }

    impl Sim {
        fn new(dst: f64, drift: f64, freq: f64) -> Self {
            Sim { p: Playout::new(), src_rate: 48000.0 * (1.0 + drift), dst_rate: dst, t_src: 0.0, frac: 0.0, phase: 0.0, freq, out: Vec::new() }
        }
        /// Pushes `ms` of source audio (a sine, 0.5 amplitude, in sender-clock time).
        fn feed(&mut self, ms: f64) {
            let exact = ms / 1000.0 * self.src_rate + self.frac;
            let n = exact.floor() as usize;
            self.frac = exact - n as f64;
            let mut v = Vec::with_capacity(n * 2);
            for _ in 0..n {
                let s = ((self.phase * TAU).sin() * 0.5 * 32767.0) as i16;
                self.phase = (self.phase + self.freq / 48000.0).fract(); // the tone is defined per nominal sample
                v.push(s);
                v.push(s);
            }
            self.t_src += n as f64 / self.src_rate;
            self.p.push(&v, 48000);
        }
        fn play(&mut self, ms: f64) {
            let n = (ms / 1000.0 * self.dst_rate).round() as usize;
            let mut b = vec![0f32; n * 2];
            self.p.render(&mut b, 2, self.dst_rate);
            self.out.extend(b.chunks_exact(2).map(|f| f[0]));
        }
    }

    /// Largest |difference| between neighbouring output samples from `from` on.
    fn max_step(v: &[f32], from: usize) -> f32 {
        v[from..].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    fn run_drift(drift: f64, secs: usize) -> Sim {
        let mut s = Sim::new(48000.0, drift, 440.0);
        s.feed(60.0);
        for i in 0..secs * 100 {
            s.feed(10.0); // every 10 ms of *sender* time...
            s.play(10.0); // ...we play 10 ms of ours
            let _ = i;
        }
        s
    }

    #[test]
    fn drift_is_absorbed_in_both_directions_without_underruns() {
        for drift in [0.0, 0.0003, -0.0003, 0.0008, -0.0008] {
            let s = run_drift(drift, 240);
            let st = s.p.stats;
            assert_eq!(st.underruns, 0, "drift {drift}: {st:?}");
            assert_eq!(st.trims, 0, "drift {drift}: {st:?}");
            // (level is read after the callback consumed 10 ms; the target shrinks from 60 to 40 ms while calm)
            assert!(st.level_ms + 10.0 >= st.target_ms - 12.0 && st.level_ms + 10.0 <= st.target_ms + 12.0, "drift {drift}: level {} ms ({st:?})", st.level_ms);
            if drift != 0.0 {
                let want = drift * 1e6;
                assert!((st.drift_ppm - want).abs() < want.abs() * 0.3 + 40.0, "drift {drift}: learned {} ppm", st.drift_ppm);
            }
        }
    }

    #[test]
    fn output_is_a_clean_sine_no_clicks() {
        // 440 Hz at 0.5: the largest natural step per output sample is 0.5*2*pi*440/48000 = 0.0288.
        let s = run_drift(0.0005, 60);
        let natural = (0.5 * TAU * 440.0 / 48000.0) as f32;
        let step = max_step(&s.out, 48000 * 3);
        assert!(step < natural * 1.1, "max step {step} vs natural {natural}");
        // and 48 -> 44.1 kHz: the natural step is a bit larger per output sample
        let mut s = Sim::new(44100.0, -0.0002, 440.0);
        s.feed(60.0);
        for _ in 0..6000 {
            s.feed(10.0);
            s.play(10.0);
        }
        let natural = (0.5 * TAU * 440.0 / 44100.0) as f32;
        let step = max_step(&s.out, 44100 * 3);
        assert!(step < natural * 1.1, "44.1k: max step {step} vs natural {natural}");
        assert_eq!(s.p.stats.underruns, 0);
    }

    #[test]
    fn kernel_interpolates_tones_accurately() {
        for (dst, f, min_db) in [(48000.0, 8000.0, 60.0), (44100.0, 5000.0, 60.0), (44100.0, 10000.0, 35.0), (96000.0, 12000.0, 30.0)] {
            let k = make_kernel(48000.0, dst);
            let mut worst = 0.0f64;
            for ph in 0..PHASES {
                let t = ph as f64 / PHASES as f64;
                let mut y = 0.0f64;
                for i in 0..TAPS {
                    y += k[ph * TAPS + i] as f64 * (TAU * f / 48000.0 * (i as f64 - HISTORY as f64) + 0.3).sin();
                }
                let want = (TAU * f / 48000.0 * t + 0.3).sin();
                worst = worst.max((y - want).abs());
            }
            let db = -20.0 * worst.log10();
            eprintln!("kernel dst {dst} f {f}: error {db:.1} dB below full scale");
            assert!(db > min_db, "dst {dst} f {f}: {db:.1} dB");
        }
    }

    #[test]
    fn underrun_fades_out_and_back_in_without_a_step() {
        let mut s = Sim::new(48000.0, 0.0, 440.0);
        s.feed(70.0);
        for _ in 0..20 {
            s.feed(10.0);
            s.play(10.0);
        }
        assert_eq!(s.p.stats.underruns, 0);
        // The source stalls for 300 ms (nothing arrives), then resumes.
        for _ in 0..30 {
            s.play(10.0);
        }
        assert!(s.p.stats.underruns >= 1, "{:?}", s.p.stats);
        let before = s.p.stats.target_ms;
        assert!(before > 60.0, "target grew to {before}");
        s.feed(120.0);
        for _ in 0..20 {
            s.feed(10.0);
            s.play(10.0);
        }
        let natural = (0.5 * TAU * 440.0 / 48000.0) as f32;
        // allow the fade slope: 1.5 * 0.5 / (0.003*48000) = 0.0052 on top of the tone's own slope
        let step = max_step(&s.out, 0);
        assert!(step < natural + 0.012, "max step {step}");
        assert!(s.out.iter().skip(20 * 480 + 20 * 480).take(480).all(|&x| x.abs() < 1e-6), "digital silence while starved");
        let tail = &s.out[s.out.len() - 480..];
        assert!(tail.iter().any(|&x| x.abs() > 0.3), "playing again");
    }

    #[test]
    fn overflow_is_trimmed_smoothly() {
        let mut s = Sim::new(48000.0, 0.0, 440.0);
        s.feed(70.0);
        s.play(20.0);
        s.feed(400.0); // a burst: 400 ms arrive at once
        for _ in 0..30 {
            s.play(10.0);
            s.feed(10.0);
        }
        assert_eq!(s.p.stats.trims, 1, "{:?}", s.p.stats);
        assert!(s.p.stats.level_ms < 100.0, "level {}", s.p.stats.level_ms);
        let natural = (0.5 * TAU * 440.0 / 48000.0) as f32;
        assert!(max_step(&s.out, 0) < natural + 0.012, "max step {}", max_step(&s.out, 0));
        assert_eq!(s.p.stats.underruns, 0);
    }

    #[test]
    fn switching_to_game_drops_the_cushion_without_a_click() {
        let mut s = Sim::new(48000.0, 0.0, 440.0);
        s.feed(100.0);
        for _ in 0..300 {
            s.feed(10.0);
            s.play(10.0);
        }
        assert!(s.p.stats.level_ms > 60.0, "movie cushion first: {:?}", s.p.stats);
        let before = s.out.len();
        s.p.set_profile(GAME);
        for _ in 0..300 {
            s.feed(10.0);
            s.play(10.0);
        }
        let st = s.p.stats;
        assert_eq!(st.underruns, 0, "{st:?}");
        assert!(st.level_ms + 10.0 <= GAME.max * 1000.0 && st.target_ms <= GAME.max * 1000.0, "{st:?}");
        let natural = (0.5 * TAU * 440.0 / 48000.0) as f32;
        assert!(max_step(&s.out, before) < natural + 0.012, "the skip is faded, not a click");
        // and back: the cushion grows again, still without underruns
        s.p.set_profile(MOVIE);
        for _ in 0..300 {
            s.feed(10.0);
            s.play(10.0);
        }
        assert!(s.p.stats.target_ms >= MOVIE.min * 1000.0 && s.p.stats.underruns == 0, "{:?}", s.p.stats);
    }

    #[test]
    fn jitter_grows_the_buffer_until_underruns_stop() {
        // Packets arrive in bursts: nothing for 90 ms, then 90 ms at once.
        let mut s = Sim::new(48000.0, 0.0, 440.0);
        s.feed(60.0);
        let mut ups = Vec::new();
        for round in 0..40 {
            for k in 0..9 {
                s.play(10.0);
                let _ = k;
            }
            s.feed(90.0);
            ups.push(s.p.stats.underruns);
            let _ = round;
        }
        let first_half = ups[19] - ups[0];
        let second_half = ups[39] - ups[19];
        assert!(s.p.stats.target_ms >= 80.0, "target {}", s.p.stats.target_ms);
        assert!(second_half <= first_half && second_half <= 1, "{ups:?}");
    }

    #[test]
    fn target_shrinks_back_when_calm() {
        let mut s = Sim::new(48000.0, 0.0, 440.0);
        s.feed(100.0);
        s.play(20.0);
        for _ in 0..30 {
            s.play(10.0);
        } // starve once
        assert!(s.p.stats.target_ms > 100.0, "{}", s.p.stats.target_ms);
        s.feed(150.0);
        for _ in 0..(CALM_SEC as usize * 100 * 3) {
            s.feed(10.0);
            s.play(10.0);
        }
        assert!(s.p.stats.target_ms <= 100.0 && s.p.stats.target_ms >= MIN_TARGET * 1000.0, "{}", s.p.stats.target_ms);
    }

    #[test]
    fn mono_and_surround_outputs_are_fed() {
        let mut p = Playout::new();
        p.push(&vec![8000i16; 8000 * 2], 48000);
        let mut o = vec![0f32; 480 * 6];
        for _ in 0..8 {
            p.render(&mut o, 6, 48000.0);
        }
        assert!(o[0] > 0.1 && o[1] > 0.1 && o[2..6].iter().all(|&x| x == 0.0));
        let mut m = vec![0f32; 480];
        p.render(&mut m, 1, 48000.0);
        assert!(m[100] > 0.1);
    }
}
