// Short sound cues (connect / disconnect / error), synthesized in memory: no files, no new audio stack.
//
// Where they may play (the host captures the computer's sound with loopback, so a cue played there would be
// heard by the friend): only the launcher window plays them, and `allowed` below says when. The viewer's
// PC plays the cue on connect; the sharing PC plays nothing while the stream runs, and only "ended" / "error"
// cues once its streaming process is gone (nothing is captured any more).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cue {
    Connect,
    Disconnect,
    Error,
}

pub const RATE: u32 = 44_100;
/// Peak level of the loudest cue (of full scale): quiet on purpose.
pub const PEAK: f32 = 0.20;

/// May this cue play on this side right now? `streaming` = this launcher has a live stream going.
pub fn allowed(cue: Cue, is_host: bool, test: bool) -> bool {
    if test {
        return false; // the self-test runs both ends on this PC: the host would capture the cue
    }
    match (cue, is_host) {
        (Cue::Connect, true) => false, // the stream has just started: loopback would carry it to the friend
        _ => true,
    }
}

/// (frequency Hz, start s, length s) of each note.
fn notes(cue: Cue) -> &'static [(f32, f32, f32)] {
    match cue {
        Cue::Connect => &[(659.25, 0.0, 0.26), (880.0, 0.11, 0.34)],
        Cue::Disconnect => &[(880.0, 0.0, 0.22), (587.33, 0.10, 0.34)],
        Cue::Error => &[(311.13, 0.0, 0.22), (233.08, 0.15, 0.36)],
    }
}

/// Mono 16-bit samples of the cue.
pub fn samples(cue: Cue) -> Vec<i16> {
    let ns = notes(cue);
    let total = ns.iter().map(|n| n.1 + n.2).fold(0.0f32, f32::max) + 0.05;
    let len = (total * RATE as f32) as usize;
    let mut mix = vec![0.0f32; len];
    for &(f, start, dur) in ns {
        let s0 = (start * RATE as f32) as usize;
        let n = (dur * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.008).min(1.0); // no click at the start
            let decay = (-t * 9.0).exp() * (1.0 - (i as f32 / n as f32).powi(4)); // and a soft end
            let ph = 2.0 * std::f32::consts::PI * f * t;
            let v = ph.sin() + 0.25 * (2.0 * ph).sin();
            if let Some(m) = mix.get_mut(s0 + i) {
                *m += v * attack * decay;
            }
        }
    }
    let peak = mix.iter().fold(0.0f32, |a, v| a.max(v.abs())).max(1e-6);
    mix.iter().map(|v| (v / peak * PEAK * i16::MAX as f32) as i16).collect()
}

/// A complete WAV file (PCM, mono, 16 bit) holding the cue.
pub fn wav(cue: Cue) -> Vec<u8> {
    let s = samples(cue);
    let data_len = (s.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + s.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&RATE.to_le_bytes());
    v.extend_from_slice(&(RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for x in s {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

/// Plays the cue without blocking (Windows `PlaySound` from memory). Errors are ignored: a missing sound
/// device must never break the app.
#[cfg(windows)]
pub fn play(cue: Cue) {
    use std::sync::OnceLock;
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    static CONNECT: OnceLock<Vec<u8>> = OnceLock::new();
    static DISCONNECT: OnceLock<Vec<u8>> = OnceLock::new();
    static ERROR: OnceLock<Vec<u8>> = OnceLock::new();
    // The buffers live for the whole run: SND_ASYNC keeps reading them after the call returns.
    let buf = match cue {
        Cue::Connect => &CONNECT,
        Cue::Disconnect => &DISCONNECT,
        Cue::Error => &ERROR,
    }
    .get_or_init(|| wav(cue));
    unsafe {
        let _ = PlaySoundW(PCWSTR(buf.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_and_length_are_consistent() {
        for c in [Cue::Connect, Cue::Disconnect, Cue::Error] {
            let w = wav(c);
            assert_eq!(&w[0..4], b"RIFF");
            assert_eq!(&w[8..16], b"WAVEfmt ");
            let riff = u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize;
            assert_eq!(riff + 8, w.len());
            let dl = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
            assert_eq!(dl + 44, w.len());
            let secs = dl as f32 / 2.0 / RATE as f32;
            assert!((0.3..1.0).contains(&secs), "{c:?} lasts {secs}s");
        }
    }

    #[test]
    fn cues_are_quiet_and_do_not_click() {
        for c in [Cue::Connect, Cue::Disconnect, Cue::Error] {
            let s = samples(c);
            let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap() as f32 / i16::MAX as f32;
            assert!(peak <= PEAK + 0.01 && peak >= PEAK * 0.9, "{c:?} peak {peak}");
            assert!(s[0].abs() < 100, "starts near zero");
            assert!(s[s.len() - 1].abs() < 100, "ends near zero");
        }
    }

    #[test]
    fn the_sharing_side_never_cues_a_connect_and_the_test_is_silent() {
        assert!(!allowed(Cue::Connect, true, false));
        assert!(allowed(Cue::Connect, false, false));
        assert!(allowed(Cue::Disconnect, true, false));
        assert!(allowed(Cue::Error, true, false));
        for c in [Cue::Connect, Cue::Disconnect, Cue::Error] {
            assert!(!allowed(c, true, true) && !allowed(c, false, true));
        }
    }
}
