// System audio: WASAPI loopback capture on the host, jitter-buffered playback on the client.
// Audio travels as raw 16-bit stereo PCM (~1.5 Mbit/s) — negligible next to the video.
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Starts capturing what the default output device plays. `on_audio(stereo_i16, sample_rate)`.
pub fn start_capture(mut on_audio: impl FnMut(&[i16], u32) + Send + 'static) -> Result<Stream> {
    let host = cpal::default_host();
    let dev = host.default_output_device().context("нет устройства вывода звука")?;
    let cfg = dev.default_output_config()?;
    let ch = cfg.channels() as usize;
    let rate = cfg.sample_rate();
    let config = cfg.config();
    let err = |e| eprintln!("звук (захват): {e}");
    let mut stereo: Vec<i16> = Vec::new();
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => dev.build_input_stream::<f32, _, _>(
            config,
            move |data, _| {
                stereo.clear();
                for f in data.chunks(ch) {
                    let l = f[0];
                    let r = if ch > 1 { f[1] } else { l };
                    stereo.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                    stereo.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
                on_audio(&stereo, rate);
            },
            err,
            None,
        )?,
        SampleFormat::I16 => dev.build_input_stream::<i16, _, _>(
            config,
            move |data, _| {
                stereo.clear();
                for f in data.chunks(ch) {
                    stereo.push(f[0]);
                    stereo.push(if ch > 1 { f[1] } else { f[0] });
                }
                on_audio(&stereo, rate);
            },
            err,
            None,
        )?,
        f => bail!("неподдерживаемый формат звука: {f:?}"),
    };
    stream.play()?;
    Ok(stream)
}

struct JitterBuf {
    /// Interleaved stereo at `src_rate`.
    q: VecDeque<f32>,
    src_rate: u32,
    playing: bool,
    pos: f64,
}

/// Start playing once this much is buffered.
const START_SEC: f64 = 0.04;
/// Level the drift control steers to.
const TARGET_SEC: f64 = 0.05;
/// Safety valve for bursts: above this, drop the oldest audio down to the start level.
const MAX_SEC: f64 = 0.25;
/// Largest playback speed correction (0.4% = ~7 cents: inaudible, fixes ±4000 ppm of clock drift).
const MAX_SPEED_FIX: f64 = 0.004;

#[derive(Clone)]
pub struct AudioQueue {
    buf: Arc<Mutex<JitterBuf>>,
    /// How long a sample pushed now takes to reach the speakers (buffer + device), in µs.
    delay_us: Arc<AtomicI64>,
}

impl AudioQueue {
    pub fn push(&self, samples: &[i16], rate: u32) {
        let mut b = self.buf.lock().unwrap();
        if b.src_rate != rate {
            b.q.clear();
            b.src_rate = rate;
            b.playing = false;
        }
        b.q.extend(samples.iter().map(|&s| s as f32 / 32768.0));
        let max = (rate as f64 * MAX_SEC) as usize * 2;
        if b.q.len() > max {
            let keep = (rate as f64 * START_SEC) as usize * 2;
            let drop = b.q.len() - keep;
            b.q.drain(..drop);
        }
    }

    /// Current audio delay on this side; the video waits this long so lips match the sound.
    pub fn delay(&self) -> Duration {
        Duration::from_micros(self.delay_us.load(Ordering::Relaxed).max(0) as u64)
    }
}

pub struct Player {
    pub queue: AudioQueue,
    _stream: Stream,
}

pub fn start_playback() -> Result<Player> {
    let host = cpal::default_host();
    let dev = host.default_output_device().context("нет устройства вывода звука")?;
    let cfg = dev.default_output_config()?;
    if cfg.sample_format() != SampleFormat::F32 {
        bail!("неподдерживаемый формат вывода звука: {:?}", cfg.sample_format());
    }
    let ch = cfg.channels() as usize;
    let dst_rate = cfg.sample_rate() as f64;
    let queue = AudioQueue {
        buf: Arc::new(Mutex::new(JitterBuf { q: VecDeque::new(), src_rate: 48000, playing: false, pos: 0.0 })),
        delay_us: Arc::new(AtomicI64::new(0)),
    };
    let q2 = queue.clone();
    let probe = crate::avsync::enabled();
    let mut beep = crate::avsync::BeepEdge::new(dst_rate as u32);
    let stream = dev.build_output_stream::<f32, _, _>(
        cfg.config(),
        move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let device_us = ts.playback.duration_since(ts.callback).as_micros() as i64;
            let t0 = if probe { crate::avsync::now_us() + device_us } else { 0 };
            let mut b = q2.buf.lock().unwrap();
            let src = b.src_rate as f64;
            let level = (b.q.len() / 2) as f64 / src;
            q2.delay_us.store((level * 1e6) as i64 + device_us, Ordering::Relaxed);
            if !b.playing && level >= START_SEC {
                b.playing = true;
            }
            // Drift control: the sender's and our sound card clocks never match exactly, so play a hair
            // faster when the buffer grows and slower when it shrinks, holding the delay steady.
            let fix = ((level - TARGET_SEC) / TARGET_SEC * MAX_SPEED_FIX).clamp(-MAX_SPEED_FIX, MAX_SPEED_FIX);
            let step = src / dst_rate * (1.0 + fix);
            for (k, f) in out.chunks_mut(ch).enumerate() {
                f.fill(0.0);
                if !b.playing || b.q.len() < 4 {
                    b.playing = b.playing && b.q.len() >= 4; // underrun: rebuffer
                    if probe {
                        beep.feed(0.0);
                    }
                    continue;
                }
                let t = b.pos as f32;
                f[0] = b.q[0] + (b.q[2] - b.q[0]) * t;
                if ch > 1 {
                    f[1] = b.q[1] + (b.q[3] - b.q[1]) * t;
                }
                if probe && beep.feed(f[0]) {
                    crate::avsync::log("client_audio", t0 + (k as f64 * 1e6 / dst_rate) as i64);
                }
                b.pos += step;
                while b.pos >= 1.0 && b.q.len() >= 2 {
                    b.q.pop_front();
                    b.q.pop_front();
                    b.pos -= 1.0;
                }
            }
        },
        |e| eprintln!("звук (вывод): {e}"),
        None,
    )?;
    stream.play()?;
    Ok(Player { queue, _stream: stream })
}
