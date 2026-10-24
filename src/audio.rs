// System audio: WASAPI loopback capture of whatever the default output device plays.
// Audio travels as raw 16-bit stereo PCM (~1.5 Mbit/s) — negligible next to the video.
use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};

/// Sample rate of the default output device (what `start_capture` will deliver).
pub fn default_rate() -> Result<u32> {
    let host = cpal::default_host();
    let dev = host.default_output_device().context("нет устройства вывода звука")?;
    Ok(dev.default_output_config()?.sample_rate())
}

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
