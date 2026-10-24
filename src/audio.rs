// System audio: WASAPI loopback capture on the host, jitter-buffered playback on the client.
// Audio travels as raw 16-bit stereo PCM (~1.5 Mbit/s) — negligible next to the video.
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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

const START_SEC: f64 = 0.04;
const MAX_SEC: f64 = 0.15;

#[derive(Clone)]
pub struct AudioQueue(Arc<Mutex<JitterBuf>>);

impl AudioQueue {
    pub fn push(&self, samples: &[i16], rate: u32) {
        let mut b = self.0.lock().unwrap();
        if b.src_rate != rate {
            b.q.clear();
            b.src_rate = rate;
            b.playing = false;
        }
        b.q.extend(samples.iter().map(|&s| s as f32 / 32768.0));
        // Too much queued (clock drift / burst): drop the oldest down to the start level.
        let max = (rate as f64 * MAX_SEC) as usize * 2;
        if b.q.len() > max {
            let keep = (rate as f64 * START_SEC) as usize * 2;
            let drop = b.q.len() - keep;
            b.q.drain(..drop);
        }
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
    let queue = AudioQueue(Arc::new(Mutex::new(JitterBuf { q: VecDeque::new(), src_rate: 48000, playing: false, pos: 0.0 })));
    let q2 = queue.clone();
    let stream = dev.build_output_stream::<f32, _, _>(
        cfg.config(),
        move |out: &mut [f32], _| {
            let mut b = q2.0.lock().unwrap();
            let step = b.src_rate as f64 / dst_rate;
            if !b.playing && b.q.len() >= (b.src_rate as f64 * START_SEC) as usize * 2 {
                b.playing = true;
            }
            for f in out.chunks_mut(ch) {
                f.fill(0.0);
                if !b.playing {
                    continue;
                }
                if b.q.len() < 4 {
                    b.playing = false; // underrun: rebuffer
                    continue;
                }
                let t = b.pos as f32;
                let l = b.q[0] + (b.q[2] - b.q[0]) * t;
                let r = b.q[1] + (b.q[3] - b.q[1]) * t;
                f[0] = l;
                if ch > 1 {
                    f[1] = r;
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
