// Decisions around the viewer's GPU colour conversion (D3D11 video processor), pure std so they are tested on
// Linux through tools/lossim. The Direct3D calls themselves live in gpuconv.rs.

/// Env var that forces the old CPU conversion (2 swscale threads), same as the `--cpu-convert` flag.
pub const FORCE_CPU_ENV: &str = "SS_CPU_CONVERT";

/// After the GPU path is (re)built for a new picture/window size, this many pictures are also converted by the
/// CPU and compared: a driver that "succeeds" but draws black or garbage is caught here, not by the user.
pub const VERIFY_FRAMES: u32 = 2;
/// Mean absolute difference (0..255, over a sparse grid of pixels, all three colour channels) above which the
/// GPU picture is considered wrong. Different scalers differ by a few units on text edges; black vs. picture is
/// far above this.
pub const MAX_MEAN_DIFF: f64 = 28.0;

/// "1", "true", "yes", "on" (any case) switch the CPU path on.
pub fn env_forces_cpu(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on" | "да"))
}

/// Bits of the legacy D3D11_VIDEO_PROCESSOR_COLOR_SPACE for a YCbCr input:
/// bit 2 YCbCr_Matrix (0 = BT.601, 1 = BT.709), bits 4-5 Nominal_Range (1 = 16-235, 2 = 0-255).
pub fn input_color_bits(bt601: bool, full_range: bool) -> u32 {
    let matrix = if bt601 { 0 } else { 1 };
    let range = if full_range { 2 } else { 1 };
    (matrix << 2) | (range << 4)
}

/// Same rule as the CPU path (video.rs Scaler): 601 only when the stream says so, else 709.
/// `spc` is the AVColorSpace value: 5 = BT470BG, 6 = SMPTE170M.
pub fn is_bt601(spc: i32) -> bool {
    spc == 5 || spc == 6
}

/// Bits for the RGB output: full range 0-255 (bit 1 RGB_Range = 0), playback usage.
pub const OUTPUT_COLOR_BITS: u32 = 0;

/// Mean absolute difference of the R, G and B channels of two same-sized 0x00RRGGBB pictures, on a grid of
/// about 64x48 sample points. Different sizes or empty pictures give infinity (= "wrong").
pub fn mean_abs_diff(a: &[u32], b: &[u32], w: usize, h: usize) -> f64 {
    if w == 0 || h == 0 || a.len() < w * h || b.len() < w * h {
        return f64::INFINITY;
    }
    let (sx, sy) = ((w / 64).max(1), (h / 48).max(1));
    let (mut sum, mut n) = (0u64, 0u64);
    let mut y = sy / 2;
    while y < h {
        let mut x = sx / 2;
        while x < w {
            let (p, q) = (a[y * w + x], b[y * w + x]);
            for sh in [0, 8, 16] {
                let (c, d) = ((p >> sh) & 255, (q >> sh) & 255);
                sum += c.abs_diff(d) as u64;
                n += 1;
            }
            x += sx;
        }
        y += sy;
    }
    sum as f64 / n.max(1) as f64
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConvPath {
    /// Not decided yet / CPU by default (no hardware frames seen).
    Cpu,
    Gpu,
    /// The GPU path failed (init, blt, readback or the picture check): permanently CPU.
    CpuAfterGpuFail,
    /// Forced by `--cpu-convert` or the env var.
    CpuForced,
}

impl ConvPath {
    pub fn name(self) -> &'static str {
        match self {
            ConvPath::Cpu => "процессор, 2 потока",
            ConvPath::Gpu => "видеокарта (D3D11 video processor)",
            ConvPath::CpuAfterGpuFail => "процессор, 2 потока (видеокарта отказала, переключился)",
            ConvPath::CpuForced => "процессор, 2 потока (принудительно)",
        }
    }
    pub fn code(self) -> u8 {
        self as u8
    }
    pub fn from_code(c: u8) -> ConvPath {
        match c {
            1 => ConvPath::Gpu,
            2 => ConvPath::CpuAfterGpuFail,
            3 => ConvPath::CpuForced,
            _ => ConvPath::Cpu,
        }
    }
}

/// The GPU path's life: starts enabled (unless forced off), any failure turns it off for good.
pub struct GpuGuard {
    path: ConvPath,
    verify_left: u32,
    reason: Option<String>,
}

impl GpuGuard {
    /// `allowed` is false when a test needs every picture in order (screenshot, A/V beacon): CPU, quietly.
    pub fn new(forced_cpu: bool, allowed: bool) -> GpuGuard {
        let path = if forced_cpu {
            ConvPath::CpuForced
        } else if allowed {
            ConvPath::Gpu
        } else {
            ConvPath::Cpu
        };
        GpuGuard { path, verify_left: VERIFY_FRAMES, reason: None }
    }

    pub fn path(&self) -> ConvPath {
        self.path
    }

    pub fn active(&self) -> bool {
        self.path == ConvPath::Gpu
    }

    /// Turns the GPU path off for good. Returns true the first time (log it once).
    pub fn fail(&mut self, why: &str) -> bool {
        if self.path == ConvPath::Gpu {
            self.path = ConvPath::CpuAfterGpuFail;
            self.reason = Some(why.to_string());
            true
        } else {
            false
        }
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The GPU objects were (re)built (first picture, new size, new device): check the next pictures again.
    pub fn rebuilt(&mut self) {
        self.verify_left = VERIFY_FRAMES;
    }

    pub fn needs_verify(&self) -> bool {
        self.active() && self.verify_left > 0
    }

    /// Feed the GPU-vs-CPU difference of a verified picture. Returns Err(text) when the GPU path got turned off.
    pub fn verified(&mut self, diff: f64) -> Result<(), String> {
        if self.verify_left > 0 {
            self.verify_left -= 1;
        }
        if diff.is_finite() && diff <= MAX_MEAN_DIFF {
            Ok(())
        } else {
            let why = format!("картинка с видеокарты не сходится с процессорной (расхождение {diff:.1} из 255)");
            self.fail(&why);
            Err(why)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values() {
        assert!(env_forces_cpu(Some("1")));
        assert!(env_forces_cpu(Some(" TRUE ")));
        assert!(env_forces_cpu(Some("on")));
        assert!(!env_forces_cpu(Some("0")));
        assert!(!env_forces_cpu(Some("")));
        assert!(!env_forces_cpu(None));
    }

    #[test]
    fn color_bits() {
        assert_eq!(input_color_bits(false, false), (1 << 2) | (1 << 4)); // 709, 16-235
        assert_eq!(input_color_bits(true, false), 1 << 4); // 601, 16-235
        assert_eq!(input_color_bits(false, true), (1 << 2) | (2 << 4)); // 709, full
        assert!(is_bt601(5) && is_bt601(6) && !is_bt601(1) && !is_bt601(2));
    }

    #[test]
    fn diff_of_equal_and_black() {
        let a: Vec<u32> = (0..320 * 200).map(|i| 0x00808080 + (i as u32 % 7)).collect();
        assert_eq!(mean_abs_diff(&a, &a, 320, 200), 0.0);
        let black = vec![0u32; 320 * 200];
        assert!(mean_abs_diff(&a, &black, 320, 200) > MAX_MEAN_DIFF);
        // a small filter difference is fine
        let b: Vec<u32> = a.iter().map(|p| p + 0x00030303).collect();
        assert!((mean_abs_diff(&a, &b, 320, 200) - 3.0).abs() < 1e-9);
        // wrong sizes / empty
        assert!(mean_abs_diff(&a, &black[..10], 320, 200).is_infinite());
        assert!(mean_abs_diff(&[], &[], 0, 0).is_infinite());
    }

    #[test]
    fn small_pictures_are_sampled() {
        let a = vec![0x00ff0000u32; 10 * 6];
        let b = vec![0x0000ff00u32; 10 * 6];
        assert_eq!(mean_abs_diff(&a, &b, 10, 6), 510.0 / 3.0);
    }

    #[test]
    fn guard_failure_is_permanent() {
        let mut g = GpuGuard::new(false, true);
        assert!(g.active() && g.path() == ConvPath::Gpu);
        assert!(g.needs_verify());
        assert!(g.verified(2.0).is_ok());
        assert!(g.verified(5.0).is_ok());
        assert!(!g.needs_verify()); // checked twice, now trusted
        assert!(g.fail("Blt: E_FAIL"));
        assert!(!g.active());
        assert_eq!(g.path(), ConvPath::CpuAfterGpuFail);
        assert_eq!(g.reason(), Some("Blt: E_FAIL"));
        assert!(!g.fail("again")); // logged only once
        g.rebuilt();
        assert!(!g.needs_verify()); // never comes back
        assert!(!g.active());
    }

    #[test]
    fn guard_catches_black_output() {
        let mut g = GpuGuard::new(false, true);
        assert!(g.verified(200.0).is_err());
        assert_eq!(g.path(), ConvPath::CpuAfterGpuFail);
        let mut g = GpuGuard::new(false, true);
        assert!(g.verified(f64::INFINITY).is_err());
    }

    #[test]
    fn guard_reverifies_after_rebuild() {
        let mut g = GpuGuard::new(false, true);
        g.verified(1.0).unwrap();
        g.verified(1.0).unwrap();
        assert!(!g.needs_verify());
        g.rebuilt(); // window resized
        assert!(g.needs_verify());
    }

    #[test]
    fn forced_and_disallowed() {
        let g = GpuGuard::new(true, true);
        assert!(!g.active());
        assert_eq!(g.path(), ConvPath::CpuForced);
        let g = GpuGuard::new(false, false);
        assert!(!g.active());
        assert_eq!(g.path(), ConvPath::Cpu);
        for p in [ConvPath::Cpu, ConvPath::Gpu, ConvPath::CpuAfterGpuFail, ConvPath::CpuForced] {
            assert_eq!(ConvPath::from_code(p.code()), p);
        }
    }
}
