// Offline checks on this PC: color accuracy through NVENC->decoder, and a real capture/encode/decode run.
use crate::capture::{Capturer, Event};
use crate::video::{Decoder, Encoder, EncoderConfig, Scaler};
use anyhow::Result;
use std::path::Path;
use std::time::{Duration, Instant};

const BARS: [(u8, u8, u8); 8] = [
    (255, 255, 255),
    (255, 255, 0),
    (0, 255, 255),
    (0, 255, 0),
    (255, 0, 255),
    (255, 0, 0),
    (0, 0, 255),
    (40, 40, 40),
];

fn rgb(p: u32) -> (u8, u8, u8) {
    ((p >> 16) as u8, (p >> 8) as u8, p as u8)
}

pub fn color_test(bitrate: u64, preset: &str, hw: bool) -> Result<()> {
    let (w, h) = (1280u32, 720u32);
    let mut enc = Encoder::new(&EncoderConfig { width: w, height: h, fps: 60, bitrate, preset: preset.into() })?;
    let mut dec = Decoder::new(hw, crate::encsel::Codec::Hevc)?;
    let mut sc = Scaler::new();
    let mut last: Vec<u32> = Vec::new();
    let mut keys = 0;
    for i in 0..30 {
        let (buf, stride) = enc.input()?;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let (r, g, b) = BARS[x * 8 / w as usize];
                buf[y * stride + x * 4..][..4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        let mut packets = Vec::new();
        enc.encode(i == 0, |d, k| packets.push((d.to_vec(), k)))?;
        for (p, k) in packets {
            keys += k as u32;
            dec.decode(&p, true, |f| {
                let _ = sc.convert(f, w, h, &mut last);
            })?;
        }
    }
    println!("Цвет (декодер: {}), ключевых кадров за 30: {keys}", if dec.hardware { "D3D11VA" } else { "CPU" });
    let mut worst = 0i32;
    for (i, &(r, g, b)) in BARS.iter().enumerate() {
        let x = (i as u32 * w / 8 + w / 16) as usize;
        let (dr, dg, db) = rgb(last[(h as usize / 2) * w as usize + x]);
        let err = (dr as i32 - r as i32).abs().max((dg as i32 - g as i32).abs()).max((db as i32 - b as i32).abs());
        worst = worst.max(err);
        println!("  ({r:3},{g:3},{b:3}) -> ({dr:3},{dg:3},{db:3})  ошибка {err}");
    }
    println!("Максимальная ошибка цвета: {worst} {}", if worst <= 8 { "— OK" } else { "— ПЛОХО" });
    Ok(())
}

/// Checks that `set_bitrate` really takes effect mid-stream (noise frames always hit the bitrate cap).
pub fn bitrate_switch_test(preset: &str) -> Result<()> {
    let (w, h) = (1280u32, 720u32);
    let mut enc = Encoder::new(&EncoderConfig { width: w, height: h, fps: 60, bitrate: 40_000_000, preset: preset.into() })?;
    let mut seed = 0x1234_5678u32;
    let mut avg = |enc: &mut Encoder, frames: u32| -> Result<f64> {
        let mut total = 0usize;
        for _ in 0..frames {
            let (buf, stride) = enc.input()?;
            for y in 0..h as usize {
                for b in &mut buf[y * stride..y * stride + w as usize * 4] {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    *b = seed as u8;
                }
            }
            enc.encode(false, |d, _| total += d.len())?;
        }
        Ok(total as f64 / frames as f64 / 1024.0)
    };
    let hi = avg(&mut enc, 60)?;
    enc.set_bitrate(8_000_000);
    let _ = avg(&mut enc, 10)?; // let the rate control settle
    let lo = avg(&mut enc, 60)?;
    println!(
        "Смена битрейта на лету: 40 Мбит/с → {hi:.0} КБ/кадр, 8 Мбит/с → {lo:.0} КБ/кадр {}",
        if lo < hi * 0.4 { "— OK" } else { "— НЕ РАБОТАЕТ" }
    );
    Ok(())
}

pub fn capture_test(monitor: crate::capture::MonitorInfo, bitrate: u64, preset: &str, hw: bool, out: &Path) -> Result<()> {
    let mut cap = Capturer::new(&monitor)?;
    let (w, h) = (cap.width, cap.height);
    println!("Захват {w}x{h} ({}), 3 секунды — подвигай мышкой/окнами...", monitor.adapter);
    let mut enc = Encoder::new(&EncoderConfig { width: w, height: h, fps: 60, bitrate, preset: preset.into() })?;
    let mut dec = Decoder::new(hw, crate::encsel::Codec::Hevc)?;
    let mut sc = Scaler::new();
    let mut img: Vec<u32> = Vec::new();
    let (mut n, mut bytes) = (0u32, 0usize);
    let (mut t_enc, mut t_dec) = (Duration::ZERO, Duration::ZERO);
    let mut max_frame = 0usize;
    let start = Instant::now();
    let mut next_due = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let timeout = next_due.saturating_duration_since(Instant::now()).as_millis() as u32;
        if let Event::Reset = cap.next(timeout)? {
            println!("захват перезапущен");
        }
        if Instant::now() < next_due {
            continue;
        }
        next_due += Duration::from_micros(16_667);
        let t0 = Instant::now();
        let (buf, stride) = enc.input()?;
        cap.compose(buf, stride);
        let mut packets = Vec::new();
        enc.encode(n == 0, |d, k| packets.push((d.to_vec(), k)))?;
        t_enc += t0.elapsed();
        let t1 = Instant::now();
        for (p, _) in &packets {
            bytes += p.len();
            max_frame = max_frame.max(p.len());
            dec.decode(p, true, |f| {
                let _ = sc.convert(f, w, h, &mut img);
            })?;
        }
        t_dec += t1.elapsed();
        n += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "кадров: {n} ({:.0}/с) | захват+кодирование: {:.2} мс | декод+конверт: {:.2} мс | поток: {:.1} Мбит/с | макс. кадр: {} КБ",
        n as f64 / secs,
        t_enc.as_secs_f64() * 1000.0 / n.max(1) as f64,
        t_dec.as_secs_f64() * 1000.0 / n.max(1) as f64,
        bytes as f64 * 8.0 / secs / 1e6,
        max_frame / 1024
    );
    if !img.is_empty() {
        crate::client::write_bmp(out, w, h, &img)?;
        println!("Снимок после кодирования/декодирования: {}", out.display());
    }
    Ok(())
}
