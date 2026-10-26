// Viewer volume: the level/mute state shared with the audio callback, the small overlay that a right click opens
// (geometry, hit-testing and drawing into a plain 0x00RRGGBB buffer), and the tiny settings file helpers.
// Pure std, no Windows calls, so it is unit-tested on any OS (tools/lossim includes this file too).
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Highest level: 150% (a bit of boost for quiet streams; the output is clamped, so it can only distort, not blow up).
pub const MAX_LEVEL: f32 = 1.5;
/// Level at start when nothing is saved.
pub const DEFAULT_LEVEL: f32 = 1.0;

/// Level and mute, shared between the window thread (sets) and the audio callback (reads).
pub struct VolumeCtl {
    bits: AtomicU32,
    muted: AtomicBool,
}

impl VolumeCtl {
    pub fn new(level: f32, muted: bool) -> VolumeCtl {
        VolumeCtl { bits: AtomicU32::new(clamp_level(level).to_bits()), muted: AtomicBool::new(muted) }
    }
    pub fn level(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn set_level(&self, v: f32) {
        self.bits.store(clamp_level(v).to_bits(), Ordering::Relaxed);
    }
    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }
    pub fn toggle_mute(&self) {
        self.muted.store(!self.muted(), Ordering::Relaxed);
    }
    /// The factor the audio callback multiplies samples by.
    pub fn gain(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            self.level()
        }
    }
}

pub fn clamp_level(v: f32) -> f32 {
    if v.is_nan() {
        DEFAULT_LEVEL
    } else {
        v.clamp(0.0, MAX_LEVEL)
    }
}

/// Applies `gain` to interleaved samples, moving smoothly from `*cur` to `gain` over the buffer (no clicks when the
/// slider jumps), and clamps to [-1, 1] (the 150% boost must never wrap or overflow into the device).
pub fn apply_gain(out: &mut [f32], channels: usize, cur: &mut f32, gain: f32) {
    let ch = channels.max(1);
    let frames = (out.len() / ch).max(1);
    let start = *cur;
    if (start - gain).abs() < 1e-4 {
        *cur = gain;
        if (gain - 1.0).abs() < 1e-4 {
            return; // 100%: leave the samples exactly as they are
        }
        for s in out.iter_mut() {
            *s = (*s * gain).clamp(-1.0, 1.0);
        }
        return;
    }
    for (i, f) in out.chunks_mut(ch).enumerate() {
        let g = start + (gain - start) * ((i + 1) as f32 / frames as f32);
        for s in f.iter_mut() {
            *s = (*s * g).clamp(-1.0, 1.0);
        }
    }
    *cur = gain;
}

// ---------------------------------------------------------------- settings file (key=value lines)

/// Value of `key` in an ini-like text.
pub fn ini_get<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then_some(v.trim())
    })
}

/// The text with `key` set to `value` (other lines are kept, so the GUI and the viewer don't wipe each other's settings).
pub fn ini_set(text: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut done = false;
    for l in text.lines() {
        if l.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
            if !done {
                out.push_str(&format!("{key}={value}\n"));
                done = true;
            }
        } else if !l.trim().is_empty() {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !done {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

/// Saved level (0..1.5) and mute from the settings text.
pub fn load(text: &str) -> (f32, bool) {
    let level = ini_get(text, "volume").and_then(|v| v.parse::<f32>().ok()).map_or(DEFAULT_LEVEL, |p| clamp_level(p / 100.0));
    let muted = ini_get(text, "muted") == Some("1");
    (level, muted)
}

pub fn percent(level: f32) -> u32 {
    (clamp_level(level) * 100.0).round() as u32
}


/// Tiny settings file shared by the GUI and the viewer: %LOCALAPPDATA%\\screenshare\\gui.ini
pub fn settings_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::PathBuf::from(base).join("screenshare").join("gui.ini"))
}

pub fn read_settings() -> String {
    settings_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default()
}

/// Sets one key in the settings file, keeping the others. Errors are ignored: settings are a convenience.
pub fn write_setting(key: &str, value: &str) {
    if let Some(p) = settings_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, ini_set(&read_settings(), key, value));
    }
}

/// Saves the level and mute for the next start.
pub fn save(v: &VolumeCtl) {
    let text = ini_set(&read_settings(), "volume", &percent(v.level()).to_string());
    let text = ini_set(&text, "muted", if v.muted() { "1" } else { "0" });
    if let Some(p) = settings_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, text);
    }
}

// ---------------------------------------------------------------- glyphs (the viewer bar draws its percent with these)

/// 3x5 digits 0-9 and '%'.
pub(crate) const GLYPH: [[u8; 5]; 11] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
    [0b101, 0b001, 0b010, 0b100, 0b101], // %
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_and_mute() {
        let v = VolumeCtl::new(0.8, false);
        assert!((v.gain() - 0.8).abs() < 1e-6);
        v.toggle_mute();
        assert_eq!(v.gain(), 0.0);
        v.toggle_mute();
        v.set_level(9.0);
        assert_eq!(v.level(), MAX_LEVEL);
        v.set_level(f32::NAN);
        assert_eq!(v.level(), DEFAULT_LEVEL);
        v.set_level(-1.0);
        assert_eq!(v.level(), 0.0);
    }

    #[test]
    fn apply_gain_unity_is_untouched_and_boost_is_clamped() {
        let mut a = [0.5f32, -0.5, 0.9, -0.9];
        let mut cur = 1.0;
        apply_gain(&mut a, 2, &mut cur, 1.0);
        assert_eq!(a, [0.5, -0.5, 0.9, -0.9]);
        let mut b = [0.5f32, -0.5, 0.9, -0.9];
        let mut cur = 1.5;
        apply_gain(&mut b, 2, &mut cur, 1.5);
        assert_eq!(b, [0.75, -0.75, 1.0, -1.0]);
    }

    #[test]
    fn apply_gain_ramps_to_target_without_a_jump() {
        let mut a = vec![1.0f32; 480 * 2];
        let mut cur = 1.0;
        apply_gain(&mut a, 2, &mut cur, 0.0);
        assert_eq!(cur, 0.0);
        assert!(a[0] > 0.99, "first sample barely changes: {}", a[0]);
        assert_eq!(a[a.len() - 1], 0.0);
        assert!(a.windows(2).all(|w| w[1] <= w[0] + 1e-6), "monotone fade");
        // both channels of a frame get the same gain
        assert_eq!(a[100], a[101]);
    }

    #[test]
    fn mute_to_unmute_ramps_up() {
        let mut a = vec![1.0f32; 200];
        let mut cur = 0.0;
        apply_gain(&mut a, 1, &mut cur, 1.0);
        assert!(a[0] < 0.05 && a[199] > 0.99);
    }

    #[test]
    fn ini_keeps_other_lines() {
        let t = "preview=0\n";
        let t = ini_set(t, "volume", "70");
        assert_eq!(t, "preview=0\nvolume=70\n");
        let t = ini_set(&t, "volume", "90");
        let t = ini_set(&t, "muted", "1");
        assert_eq!(t, "preview=0\nvolume=90\nmuted=1\n");
        assert_eq!(ini_get(&t, "preview"), Some("0"));
        assert_eq!(ini_get(&t, "nope"), None);
        // duplicated key collapses to one
        assert_eq!(ini_set("volume=1\nvolume=2\n", "volume", "3"), "volume=3\n");
    }

    #[test]
    fn load_defaults_and_values() {
        assert_eq!(load(""), (1.0, false));
        assert_eq!(load("volume=70\nmuted=1\n"), (0.7, true));
        assert_eq!(load("volume=999\n").0, MAX_LEVEL);
        assert_eq!(load("volume=abc\n").0, DEFAULT_LEVEL);
        assert_eq!(load("volume=0\n"), (0.0, false));
    }

    #[test]
    fn percent_text() {
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(1.5), 150);
        assert_eq!(percent(0.0), 0);
        assert_eq!(percent(0.755), 76);
    }
}
