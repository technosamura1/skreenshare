// System audio: WASAPI loopback capture on the host, jitter-buffered playback on the client.
// Audio travels as raw 16-bit stereo PCM (~1.5 Mbit/s) — negligible next to the video.
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use crate::playout::{PlayStats, Playout};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
                    stereo.push((l.clamp(-1.0, 1.0) * 32767.0).round() as i16);
                    stereo.push((r.clamp(-1.0, 1.0) * 32767.0).round() as i16);
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

/// Capture-side glitches seen on this machine (WASAPI DATA_DISCONTINUITY: samples were lost before we read them).
pub static CAPTURE_GLITCHES: AtomicU64 = AtomicU64::new(0);

/// Puts the calling (audio) thread into the Windows "Pro Audio" MMCSS class, so a busy game or encoder
/// cannot starve it (starved audio threads are a classic source of crackle). Falls back to the highest
/// plain priority when MMCSS refuses.
pub fn boost_audio_thread() {
    use windows::core::w;
    use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
    unsafe {
        let mut task = 0u32;
        if AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task).is_err() {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        }
    }
}

#[derive(Clone)]
pub struct AudioQueue {
    buf: Arc<Mutex<Playout>>,
    /// How long a sample pushed now takes to reach the speakers (buffer + device), in µs.
    delay_us: Arc<AtomicI64>,
}

impl AudioQueue {
    pub fn push(&self, samples: &[i16], rate: u32) {
        self.buf.lock().unwrap().push(samples, rate);
    }

    /// Current audio delay on this side; the video waits this long so lips match the sound.
    pub fn delay(&self) -> Duration {
        Duration::from_micros(self.delay_us.load(Ordering::Relaxed).max(0) as u64)
    }

    /// Playback health counters for viewer.log.
    pub fn stats(&self) -> PlayStats {
        self.buf.lock().unwrap().stats
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
    let queue = AudioQueue { buf: Arc::new(Mutex::new(Playout::new())), delay_us: Arc::new(AtomicI64::new(0)) };
    let q2 = queue.clone();
    let probe = crate::avsync::enabled();
    let mut beep = crate::avsync::BeepEdge::new(dst_rate as u32);
    let mut boosted = false;
    let stream = dev.build_output_stream::<f32, _, _>(
        cfg.config(),
        move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
            if !boosted {
                boosted = true;
                boost_audio_thread();
            }
            let ts = info.timestamp();
            let device_us = ts.playback.duration_since(ts.callback).as_micros() as i64;
            let t0 = if probe { crate::avsync::now_us() + device_us } else { 0 };
            {
                let mut b = q2.buf.lock().unwrap();
                q2.delay_us.store((b.level_sec() * 1e6) as i64 + device_us, Ordering::Relaxed);
                b.render(out, ch, dst_rate);
            }
            if probe {
                for (k, f) in out.chunks(ch).enumerate() {
                    if beep.feed(f[0]) {
                        crate::avsync::log("client_audio", t0 + (k as f64 * 1e6 / dst_rate) as i64);
                    }
                }
            }
        },
        |e| eprintln!("звук (вывод): {e}"),
        None,
    )?;
    stream.play()?;
    Ok(Player { queue, _stream: stream })
}
