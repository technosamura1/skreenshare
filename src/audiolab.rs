// Audio quality lab (test tooling, CLI only): `audiolab tone` plays a pure sine, `audiolab record --pid N`
// records exactly what one process (the viewer) renders, `audiolab analyze` finds what an ear would call
// crackles, clicks and dropouts in that recording. A pure tone makes every defect measurable: a clean sine
// obeys x[n] = 2cos(w)·x[n-1] − x[n-2], so the prediction residual is ~0 and any step, gap or glitch sticks out.
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn tone(secs: u64, freq: f64, amp: f32) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let dev = cpal::default_host().default_output_device().context("нет устройства вывода звука")?;
    let cfg = dev.default_output_config()?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        bail!("формат вывода {:?} не поддерживается", cfg.sample_format());
    }
    let ch = cfg.channels() as usize;
    let sr = cfg.sample_rate() as f64;
    let mut phase = 0.0f64;
    let stream = dev.build_output_stream::<f32, _, _>(
        cfg.config(),
        move |out: &mut [f32], _| {
            for f in out.chunks_mut(ch) {
                f.fill((phase * std::f64::consts::TAU).sin() as f32 * amp);
                phase = (phase + freq / sr).fract();
            }
        },
        |e| eprintln!("тон: {e}"),
        None,
    )?;
    stream.play()?;
    std::thread::sleep(Duration::from_secs(secs));
    Ok(())
}

pub fn record(pid: u32, secs: u64, out: &Path) -> Result<()> {
    let buf = Arc::new(Mutex::new(Vec::<i16>::new()));
    let b2 = buf.clone();
    let cap = crate::loopback::start(pid, crate::loopback::Mode::Include, move |s, _| {
        b2.lock().unwrap().extend_from_slice(s);
    })?;
    std::thread::sleep(Duration::from_secs(secs));
    drop(cap);
    let samples = buf.lock().unwrap();
    write_wav(out, &samples, crate::loopback::RATE)?;
    println!("записано {:.1} с в {}", samples.len() as f64 / 2.0 / crate::loopback::RATE as f64, out.display());
    Ok(())
}

fn write_wav(path: &Path, s: &[i16], rate: u32) -> std::io::Result<()> {
    let data = (s.len() * 2) as u32;
    let mut f = Vec::with_capacity(44 + data as usize);
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&data.to_le_bytes());
    for v in s {
        f.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, f)
}

fn read_wav_left(path: &Path) -> Result<(Vec<f64>, f64)> {
    let b = std::fs::read(path)?;
    if b.len() < 44 || &b[0..4] != b"RIFF" {
        bail!("не WAV");
    }
    let rate = u32::from_le_bytes(b[24..28].try_into().unwrap()) as f64;
    let x = b[44..].chunks_exact(4).map(|c| i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0).collect();
    Ok((x, rate))
}

pub struct Report {
    pub seconds: f64,
    pub freq_ppm: f64,
    pub clicks: Vec<f64>,
    pub dropouts: usize,
    pub dropout_ms: f64,
    pub floor_db: f64,
    pub level_min: f64,
    pub level_max: f64,
}

/// Skips the first `skip` seconds (start-up buffering) of the recording.
pub fn analyze(path: &Path, freq: f64, skip: f64) -> Result<Report> {
    let (all, rate) = read_wav_left(path)?;
    let start = (skip * rate) as usize;
    let end = all.len().saturating_sub((0.3 * rate) as usize);
    if end <= start + rate as usize {
        bail!("запись слишком короткая");
    }
    let x = &all[start..end];
    let rms = |s: &[f64]| (s.iter().map(|v| v * v).sum::<f64>() / s.len().max(1) as f64).sqrt();
    let nominal = rms(x);
    if nominal < 1e-4 {
        bail!("в записи тишина");
    }
    let amp = nominal * std::f64::consts::SQRT_2;

    // Actual pitch from zero crossings (the viewer's drift control may play a hair faster or slower).
    let mut first = None;
    let mut last = 0.0;
    let mut n = 0usize;
    for i in 1..x.len() {
        if x[i - 1] < 0.0 && x[i] >= 0.0 {
            let t = (i - 1) as f64 + x[i - 1] / (x[i - 1] - x[i]);
            if first.is_none() {
                first = Some(t);
            } else {
                n += 1;
            }
            last = t;
        }
    }
    let f_est = if n > 0 { n as f64 * rate / (last - first.unwrap()) } else { freq };
    let w = std::f64::consts::TAU * f_est / rate;
    let c = 2.0 * w.cos();

    // Residual of the sine predictor, relative to the tone's amplitude.
    let r: Vec<f64> = (2..x.len()).map(|i| (x[i] - c * x[i - 1] + x[i - 2]).abs() / amp).collect();
    let mut sorted = r.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = sorted[sorted.len() / 2].max(1e-9);

    // A click: residual above −26 dB of the tone; events closer than 20 ms are one click.
    let mut clicks = Vec::new();
    let mut last_click = f64::NEG_INFINITY;
    for (i, &v) in r.iter().enumerate() {
        if v > 0.05 {
            let t = skip + (i + 2) as f64 / rate;
            if t - last_click > 0.020 {
                clicks.push(t);
            }
            last_click = t;
        }
    }

    // Dropouts and level wobble on 5 ms / 100 ms windows.
    let w5 = (0.005 * rate) as usize;
    let mut dropouts = 0;
    let mut dropout_win = 0;
    let mut in_drop = false;
    for win in x.chunks(w5) {
        let low = rms(win) < nominal * 0.5;
        if low {
            dropout_win += 1;
            if !in_drop {
                dropouts += 1;
            }
        }
        in_drop = low;
    }
    let levels: Vec<f64> = x.chunks((0.1 * rate) as usize).filter(|c| c.len() > 100).map(|c| rms(c) / nominal).collect();
    Ok(Report {
        seconds: x.len() as f64 / rate,
        freq_ppm: (f_est / freq - 1.0) * 1e6,
        clicks,
        dropouts,
        dropout_ms: dropout_win as f64 * 5.0,
        floor_db: 20.0 * floor.log10(),
        level_min: levels.iter().cloned().fold(f64::INFINITY, f64::min),
        level_max: levels.iter().cloned().fold(0.0, f64::max),
    })
}

pub fn print_report(name: &str, r: &Report) {
    let first: Vec<String> = r.clicks.iter().take(8).map(|t| format!("{t:.2}")).collect();
    println!(
        "{name}: {:.1} с · щелчков {} (на минуту {:.1}){} · провалов {} ({:.0} мс) · шум {:.0} дБ · громкость {:.2}..{:.2} · высота {:+.0} ppm",
        r.seconds,
        r.clicks.len(),
        r.clicks.len() as f64 * 60.0 / r.seconds,
        if first.is_empty() { String::new() } else { format!(" [с: {}]", first.join(", ")) },
        r.dropouts,
        r.dropout_ms,
        r.floor_db,
        r.level_min,
        r.level_max,
        r.freq_ppm,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, f: f64, rate: f64) -> Vec<i16> {
        (0..n).flat_map(|i| {
            let v = ((i as f64 * std::f64::consts::TAU * f / rate).sin() * 3000.0) as i16;
            [v, v]
        }).collect()
    }

    #[test]
    fn clean_tone_has_no_defects_and_a_gap_is_found() {
        let dir = std::env::temp_dir();
        let clean = dir.join("audiolab_clean.wav");
        write_wav(&clean, &sine(48_000 * 4, 440.0, 48_000.0), 48_000).unwrap();
        let r = analyze(&clean, 440.0, 0.5).unwrap();
        assert!(r.clicks.is_empty(), "{:?}", r.clicks);
        assert_eq!(r.dropouts, 0);
        assert!(r.freq_ppm.abs() < 50.0);

        let mut s = sine(48_000 * 4, 440.0, 48_000.0);
        for v in &mut s[2 * 96_000..2 * 96_000 + 960] {
            *v = 0; // a 10 ms hole at 2.0 s
        }
        let holed = dir.join("audiolab_hole.wav");
        write_wav(&holed, &s, 48_000).unwrap();
        let r = analyze(&holed, 440.0, 0.5).unwrap();
        assert!(!r.clicks.is_empty() && (r.clicks[0] - 2.0).abs() < 0.02, "{:?}", r.clicks);
        assert!(r.dropouts >= 1);
    }
}
