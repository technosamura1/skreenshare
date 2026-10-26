// Which audio to capture: only the shown window's process, everything but Discord, or everything but our own app.
// Pure choice logic (tools/lossim tests it); the Windows calls live in host.rs / loopback.rs.

/// User choices (the two checkboxes). Carried to the host as `--audio-all` / `--keep-voice` at start and as the
/// control line `AUDIO window=1 voice=1` while streaming.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioPrefs {
    /// "Звук только из этого окна": when a window is shown, capture that window's process tree only.
    pub window_only: bool,
    /// "Не передавать звук Discord": leave a running Discord/Vesktop out of the captured sound.
    pub exclude_voice: bool,
}

impl Default for AudioPrefs {
    fn default() -> Self {
        AudioPrefs { window_only: true, exclude_voice: true }
    }
}

impl AudioPrefs {
    pub fn line(&self) -> String {
        format!("AUDIO window={} voice={}", self.window_only as u8, self.exclude_voice as u8)
    }

    /// Parses `AUDIO window=0|1 voice=0|1`; missing fields keep `self`'s value.
    pub fn parse_line(&self, line: &str) -> Option<AudioPrefs> {
        let rest = line.trim().strip_prefix("AUDIO")?;
        let mut p = *self;
        for kv in rest.split_whitespace() {
            let (k, v) = kv.split_once('=')?;
            let on = match v {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            match k {
                "window" => p.window_only = on,
                "voice" => p.exclude_voice = on,
                _ => return None,
            }
        }
        Some(p)
    }
}

/// What the capture should be aimed at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioPlan {
    /// Only this process and its children (WASAPI process loopback, "include target process tree").
    OnlyProcess(u32),
    /// Everything except this process tree (Discord, or our own app so the viewer's playback does not loop back).
    AllExcept(u32),
}

impl AudioPlan {
    pub fn pid(&self) -> u32 {
        match self {
            AudioPlan::OnlyProcess(p) | AudioPlan::AllExcept(p) => *p,
        }
    }
}

/// `window_pid`: the process owning the window being shown (None: a whole screen is shown, or it is unknown).
/// `voice_pid`: a running Discord-like app. `own_pid`: this program.
pub fn choose(prefs: AudioPrefs, window_pid: Option<u32>, voice_pid: Option<u32>, own_pid: u32) -> AudioPlan {
    if prefs.window_only {
        if let Some(p) = window_pid.filter(|&p| p != 0 && p != own_pid) {
            return AudioPlan::OnlyProcess(p);
        }
    }
    match voice_pid {
        Some(v) if prefs.exclude_voice => AudioPlan::AllExcept(v),
        _ => AudioPlan::AllExcept(own_pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: u32 = 100;

    fn p(window_only: bool, exclude_voice: bool) -> AudioPrefs {
        AudioPrefs { window_only, exclude_voice }
    }

    #[test]
    fn window_with_checkbox_captures_only_that_process() {
        assert_eq!(choose(p(true, true), Some(4242), None, OWN), AudioPlan::OnlyProcess(4242));
        // Discord running does not matter: only the window's sound is taken
        assert_eq!(choose(p(true, true), Some(4242), Some(777), OWN), AudioPlan::OnlyProcess(4242));
    }

    #[test]
    fn full_screen_keeps_system_sound_without_discord_or_ourselves() {
        assert_eq!(choose(p(true, true), None, None, OWN), AudioPlan::AllExcept(OWN));
        assert_eq!(choose(p(true, true), None, Some(777), OWN), AudioPlan::AllExcept(777));
        assert_eq!(choose(p(true, false), None, Some(777), OWN), AudioPlan::AllExcept(OWN));
    }

    #[test]
    fn checkbox_off_means_system_sound_even_for_a_window() {
        assert_eq!(choose(p(false, true), Some(4242), None, OWN), AudioPlan::AllExcept(OWN));
        assert_eq!(choose(p(false, true), Some(4242), Some(777), OWN), AudioPlan::AllExcept(777));
    }

    #[test]
    fn never_targets_ourselves_or_pid_zero() {
        assert_eq!(choose(p(true, true), Some(OWN), None, OWN), AudioPlan::AllExcept(OWN));
        assert_eq!(choose(p(true, true), Some(0), None, OWN), AudioPlan::AllExcept(OWN));
    }

    #[test]
    fn switching_source_changes_the_plan() {
        let a = choose(p(true, true), Some(4242), Some(777), OWN);
        let b = choose(p(true, true), Some(5555), Some(777), OWN);
        let c = choose(p(true, true), None, Some(777), OWN);
        assert!(a != b && b != c && a != c);
        assert_eq!(a, choose(p(true, true), Some(4242), Some(777), OWN)); // stable: no restart loop
    }

    #[test]
    fn prefs_line_round_trips() {
        for w in [false, true] {
            for v in [false, true] {
                let x = p(w, v);
                assert_eq!(AudioPrefs::default().parse_line(&x.line()), Some(x));
            }
        }
        assert_eq!(p(true, true).parse_line("AUDIO window=0"), Some(p(false, true)));
        assert_eq!(p(true, true).parse_line("AUDIO window=2"), None);
        assert_eq!(p(true, true).parse_line("SOURCE window 5"), None);
    }
}
