// Click-free loss concealment for interleaved stereo i16 (pure std; tested through tools/lossim).
//
// A hole in the audio used to be filled with "the last chunk, faded out": its first sample is not the one
// that follows the last played sample, so every concealment started with a click (a step), and the audio
// that came after the hole started with another one. Here:
//  - the hole is filled by *continuing the waveform*: the last pitch period (found by correlation) is
//    repeated, so the first synthetic sample follows the last real one smoothly; it fades out slowly;
//  - the synthetic signal keeps running for a short `tail` past the hole and is cross-faded into the real
//    audio that comes next, so there is no step at the other end either;
//  - a chunk rebuilt from the 8-bit copy (a-law/mu-law error up to a few %) is joined to its neighbours
//    by a short ramp that removes the step at both seams.

/// Frames (stereo) of recent history kept by the caller / needed for the search.
pub const HIST_FRAMES: usize = 1100;
/// Cross-fade length in frames (2 ms at 48 kHz).
pub const TAIL_FRAMES: usize = 96;
const MIN_LAG: usize = 96; // 2 ms: pitch up to 500 Hz (higher tones repeat as several periods)
const MAX_LAG: usize = 720; // 15 ms
const WIN: usize = 240; // correlation window, 5 ms
/// The synthetic signal fades to silence over this many frames (15 ms): longer holes aren't worth guessing.
const FADE_FRAMES: usize = 720;
/// Ramp that fixes a seam step, frames.
const SEAM_FRAMES: usize = 48;

fn to_i16(v: f32) -> i16 {
    v.round().clamp(-32768.0, 32767.0) as i16
}

/// Best repeat period (in frames) for the end of `hist`, or None if the history is silent / too short.
fn find_lag(hist: &[i16]) -> Option<usize> {
    let n = hist.len() / 2;
    if n < WIN + MIN_LAG + 8 {
        return None;
    }
    let max_lag = MAX_LAG.min(n - WIN);
    let mono: Vec<f32> = hist.chunks_exact(2).map(|f| (f[0] as f32 + f[1] as f32) * 0.5).collect();
    let a = &mono[n - WIN..];
    let ea: f32 = a.iter().map(|x| x * x).sum();
    if ea < 1.0 * WIN as f32 {
        return None; // silence
    }
    let mut best = (0.0f32, MIN_LAG);
    for lag in MIN_LAG..=max_lag {
        let b = &mono[n - WIN - lag..n - lag];
        let (mut c, mut eb) = (0.0f32, 1e-3f32);
        for i in 0..WIN {
            c += a[i] * b[i];
            eb += b[i] * b[i];
        }
        let score = c / (ea * eb).sqrt();
        if score > best.0 {
            best = (score, lag);
        }
    }
    (best.0 > 0.1).then_some(best.1)
}

/// Synthesizes `gap` frames that continue `hist` and `tail` more frames that go on after them (for the
/// cross-fade into the real audio, see `blend_in`). Both are interleaved stereo.
pub fn conceal(hist: &[i16], gap: usize, tail: usize) -> (Vec<i16>, Vec<i16>) {
    let total = gap + tail;
    let mut out = vec![0i16; total * 2];
    if let Some(lag) = find_lag(hist) {
        let n = hist.len() / 2;
        let base = n - lag;
        for t in 0..total.min(FADE_FRAMES) {
            let g = 1.0 - t as f32 / FADE_FRAMES as f32;
            let s = (base + t % lag) * 2;
            out[t * 2] = to_i16(hist[s] as f32 * g);
            out[t * 2 + 1] = to_i16(hist[s + 1] as f32 * g);
        }
    }
    let tail_v = out.split_off(gap * 2);
    (out, tail_v)
}

/// Cross-fades the start of `real` from the synthetic `tail` (which ran on past the hole) into the real audio.
pub fn blend_in(real: &mut [i16], tail: &[i16]) {
    let m = (real.len().min(tail.len()) / 2).max(1);
    for i in 0..m {
        let w = (i as f32 + 0.5) / m as f32;
        for c in 0..2 {
            if let (Some(r), Some(t)) = (real.get_mut(i * 2 + c), tail.get(i * 2 + c)) {
                *r = to_i16(*r as f32 * w + *t as f32 * (1.0 - w));
            }
        }
    }
}

/// Removes the step between the last played samples (`hist`) and the start of `chunk` (which came from a
/// lossy copy): the first sample is moved to where the played signal was heading, the offset fades out.
pub fn match_start(hist: &[i16], chunk: &mut [i16]) {
    let hn = hist.len() / 2;
    if hn < 2 || chunk.len() < 4 {
        return;
    }
    let m = SEAM_FRAMES.min(chunk.len() / 2);
    for c in 0..2 {
        let (p1, p0) = (hist[(hn - 1) * 2 + c] as f32, hist[(hn - 2) * 2 + c] as f32);
        let pred = p1 + (p1 - p0);
        let d = pred - chunk[c] as f32;
        for i in 0..m {
            let k = chunk[i * 2 + c] as f32 + d * (1.0 - i as f32 / m as f32);
            chunk[i * 2 + c] = to_i16(k);
        }
    }
}

/// Removes the step between the end of `chunk` (lossy copy) and `next`, the exact audio that follows it.
pub fn match_end(chunk: &mut [i16], next: &[i16]) {
    if chunk.len() < 4 || next.len() < 4 {
        return;
    }
    let n = chunk.len() / 2;
    let m = SEAM_FRAMES.min(n);
    for c in 0..2 {
        let (n0, n1) = (next[c] as f32, next[2 + c] as f32);
        let pred_prev = n0 - (n1 - n0);
        let d = pred_prev - chunk[(n - 1) * 2 + c] as f32;
        for i in 0..m {
            let idx = (n - m + i) * 2 + c;
            chunk[idx] = to_i16(chunk[idx] as f32 + d * (i + 1) as f32 / m as f32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, start: usize, freq: f32, amp: f32) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let v = (((start + i) as f32 * freq * std::f32::consts::TAU / 48000.0).sin() * amp) as i16;
                [v, v / 2]
            })
            .collect()
    }

    fn max_step(v: &[i16]) -> i32 {
        v.chunks_exact(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] as i32 - w[0][0] as i32).abs()).max().unwrap()
    }

    #[test]
    fn concealment_continues_a_tone_without_a_step() {
        for freq in [110.0f32, 440.0, 1000.0, 3000.0] {
            let hist = sine(HIST_FRAMES, 0, freq, 12000.0);
            let (gap, tail) = conceal(&hist, 240, TAIL_FRAMES);
            assert_eq!((gap.len(), tail.len()), (480, TAIL_FRAMES * 2));
            let natural = max_step(&sine(HIST_FRAMES, 0, freq, 12000.0));
            let mut joined = hist[hist.len() - 40..].to_vec();
            joined.extend_from_slice(&gap);
            joined.extend_from_slice(&tail);
            let step = max_step(&joined);
            assert!(step as f32 <= natural as f32 * 1.4 + 60.0, "{freq} Hz: step {step} vs natural {natural}");
            // and it sounds like the tone, not silence
            let e: f64 = gap.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / gap.len() as f64;
            assert!(e.sqrt() > 4000.0, "{freq} Hz: rms {}", e.sqrt());
        }
    }

    #[test]
    fn crossfade_back_into_real_audio_is_smooth_and_close() {
        let hist = sine(HIST_FRAMES, 0, 440.0, 12000.0);
        let (_, tail) = conceal(&hist, 240, TAIL_FRAMES);
        let mut next = sine(120, HIST_FRAMES + 240, 440.0, 12000.0);
        let orig = next.clone();
        blend_in(&mut next, &tail);
        assert!(max_step(&next) <= max_step(&orig) * 2, "{} vs {}", max_step(&next), max_step(&orig));
        assert_eq!(next[TAIL_FRAMES * 2..], orig[TAIL_FRAMES * 2..], "everything after the fade is untouched");
        let d: i32 = (0..TAIL_FRAMES * 2).map(|i| (next[i] as i32 - orig[i] as i32).abs()).max().unwrap();
        assert!(d < 4500, "a continued tone (fading out) is close to the real one (max diff {d})");
    }

    #[test]
    fn silence_and_short_history_give_silence() {
        let (g, t) = conceal(&vec![0i16; 2000], 100, 50);
        assert!(g.iter().chain(t.iter()).all(|&x| x == 0));
        let (g, _) = conceal(&sine(50, 0, 440.0, 9000.0), 100, 50);
        assert!(g.iter().all(|&x| x == 0));
    }

    #[test]
    fn long_hole_fades_out() {
        let hist = sine(HIST_FRAMES, 0, 440.0, 12000.0);
        let (gap, _) = conceal(&hist, 2000, 0);
        let tail_rms = |s: &[i16]| (s.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt();
        assert!(tail_rms(&gap[..400]) > 3.0 * tail_rms(&gap[gap.len() - 800..gap.len() - 400]) + 1.0);
        assert!(gap[gap.len() - 200..].iter().all(|&x| x == 0), "silence after the fade");
    }

    #[test]
    fn seam_matching_removes_steps() {
        let full = sine(400, 0, 300.0, 10000.0);
        let hist = full[..200 * 2].to_vec();
        let mut chunk = full[200 * 2..300 * 2].to_vec();
        for (i, s) in chunk.iter_mut().enumerate() {
            *s = s.saturating_add(if i % 2 == 0 { 450 } else { -300 }); // lossy copy error
        }
        match_start(&hist, &mut chunk);
        let mut j = hist[hist.len() - 20..].to_vec();
        j.extend_from_slice(&chunk);
        let next = full[300 * 2..].to_vec();
        match_end(&mut chunk, &next);
        let mut j2 = chunk[chunk.len() - 20..].to_vec();
        j2.extend_from_slice(&next[..20]);
        let natural = max_step(&full);
        assert!(max_step(&j) <= natural + 150, "start seam {}", max_step(&j));
        assert!(max_step(&j2) <= natural + 150, "end seam {}", max_step(&j2));
    }
}
