// Thin safe wrapper over the vendored libopus (compiled in by build.rs): 48 kHz stereo, 10 ms frames,
// inband FEC on the encoder, built-in packet-loss concealment on the decoder.
use anyhow::{bail, Result};
use std::os::raw::{c_int, c_uchar};

#[repr(C)]
struct OpusEncoder([u8; 0]);
#[repr(C)]
struct OpusDecoder([u8; 0]);

extern "C" {
    fn opus_encoder_create(fs: i32, channels: c_int, application: c_int, error: *mut c_int) -> *mut OpusEncoder;
    fn opus_encoder_destroy(e: *mut OpusEncoder);
    fn opus_encode(e: *mut OpusEncoder, pcm: *const i16, frame_size: c_int, data: *mut c_uchar, max: i32) -> i32;
    fn opus_encoder_ctl(e: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decoder_destroy(d: *mut OpusDecoder);
    fn opus_decode(d: *mut OpusDecoder, data: *const c_uchar, len: i32, pcm: *mut i16, frame_size: c_int, fec: c_int) -> c_int;
}

const OPUS_APPLICATION_AUDIO: c_int = 2049;
const OPUS_SET_BITRATE: c_int = 4002;
const OPUS_SET_COMPLEXITY: c_int = 4010;
const OPUS_SET_INBAND_FEC: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC: c_int = 4014;
const OPUS_SET_VBR: c_int = 4006;
const OPUS_SET_SIGNAL: c_int = 4024;
const OPUS_SIGNAL_MUSIC: c_int = 3002;

/// Samples per channel in one packet: 10 ms at 48 kHz.
pub const FRAME: usize = 480;
pub const RATE: u32 = 48_000;

pub struct Encoder(*mut OpusEncoder);
unsafe impl Send for Encoder {}

impl Encoder {
    /// `kbps`: target bitrate; the packet loss hint makes the codec spend bits on FEC.
    pub fn new(kbps: i32, expected_loss_percent: i32) -> Result<Encoder> {
        unsafe {
            let mut err = 0;
            let e = opus_encoder_create(RATE as i32, 2, OPUS_APPLICATION_AUDIO, &mut err);
            if e.is_null() || err != 0 {
                bail!("opus_encoder_create: {err}");
            }
            opus_encoder_ctl(e, OPUS_SET_BITRATE, kbps * 1000);
            opus_encoder_ctl(e, OPUS_SET_VBR, 0 as c_int); // constant size: predictable bandwidth, simpler pacing
            opus_encoder_ctl(e, OPUS_SET_COMPLEXITY, 8 as c_int);
            opus_encoder_ctl(e, OPUS_SET_INBAND_FEC, 1 as c_int);
            opus_encoder_ctl(e, OPUS_SET_PACKET_LOSS_PERC, expected_loss_percent as c_int);
            opus_encoder_ctl(e, OPUS_SET_SIGNAL, OPUS_SIGNAL_MUSIC); // game/video audio: don't over-favour speech
            Ok(Encoder(e))
        }
    }

    #[allow(dead_code)]
    pub fn set_loss_percent(&mut self, p: i32) {
        unsafe {
            opus_encoder_ctl(self.0, OPUS_SET_PACKET_LOSS_PERC, p as c_int);
        }
    }

    /// Encodes exactly `FRAME` stereo frames (960 interleaved samples).
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<()> {
        assert_eq!(pcm.len(), FRAME * 2);
        let mut buf = [0u8; 1275];
        let n = unsafe { opus_encode(self.0, pcm.as_ptr(), FRAME as c_int, buf.as_mut_ptr(), buf.len() as i32) };
        if n < 0 {
            bail!("opus_encode: {n}");
        }
        out.clear();
        out.extend_from_slice(&buf[..n as usize]);
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0) }
    }
}

pub struct Decoder(*mut OpusDecoder);
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Decoder> {
        unsafe {
            let mut err = 0;
            let d = opus_decoder_create(RATE as i32, 2, &mut err);
            if d.is_null() || err != 0 {
                bail!("opus_decoder_create: {err}");
            }
            Ok(Decoder(d))
        }
    }

    /// Decodes one packet into `out` (appended, 960 samples). `None` = the packet is lost: conceal.
    /// `fec` = decode the *previous* frame out of this packet's inband FEC data instead.
    pub fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut Vec<i16>) -> Result<()> {
        let start = out.len();
        out.resize(start + FRAME * 2, 0);
        let (p, n) = match packet {
            Some(d) => (d.as_ptr(), d.len() as i32),
            None => (std::ptr::null(), 0),
        };
        let r = unsafe { opus_decode(self.0, p, n, out[start..].as_mut_ptr(), FRAME as c_int, fec as c_int) };
        if r < 0 {
            out.truncate(start);
            bail!("opus_decode: {r}");
        }
        out.truncate(start + r as usize * 2);
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.0) }
    }
}

/// `opustest`: encode + decode a tone through the bundled libopus, check it survives, and time the encoder.
pub fn selftest() -> Result<()> {
    let mut enc = Encoder::new(128, 10)?;
    let mut dec = Decoder::new()?;
    let (mut pkt, mut pcm, mut all_in, mut all_out) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let t = std::time::Instant::now();
    let frames = 300; // 3 s
    for n in 0..frames {
        let input: Vec<i16> = (0..FRAME)
            .flat_map(|i| {
                let v = (((n * FRAME + i) as f32 * 1000.0 * std::f32::consts::TAU / RATE as f32).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect();
        enc.encode(&input, &mut pkt)?;
        dec.decode(Some(&pkt), false, &mut pcm)?;
        all_in.extend_from_slice(&input);
        all_out.extend_from_slice(&pcm);
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    let skip = RATE as usize / 5 * 2; // the codec's start-up
    let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    let (a, b) = (rms(&all_in[skip..]), rms(&all_out[skip..]));
    println!("Opus: {} байт на 10 мс кадр, уровень вход {a:.0} / выход {b:.0}, кодек+декодер {ms:.3} мс на кадр", pkt.len());
    if (b / a - 1.0).abs() > 0.1 {
        bail!("Opus искажает сигнал");
    }
    println!("OK: Opus работает");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, f0: f32, start: usize) -> Vec<i16> {
        (0..frames).flat_map(|i| {
            let v = (((i + start) as f32 * f0 * std::f32::consts::TAU / 48000.0).sin() * 9000.0) as i16;
            [v, v]
        }).collect()
    }

    #[test]
    fn roundtrip_conceal_and_fec_work() {
        let mut enc = Encoder::new(128, 10).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut pk = Vec::new();
        let mut packets: Vec<Vec<u8>> = Vec::new();
        for k in 0..40 {
            enc.encode(&tone(FRAME, 440.0, k * FRAME), &mut pk).unwrap();
            assert!(pk.len() > 20 && pk.len() < 400, "packet {} bytes", pk.len());
            packets.push(pk.clone());
        }
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        // plain decode of everything
        let mut out = Vec::new();
        for p in &packets {
            dec.decode(Some(p), false, &mut out).unwrap();
        }
        assert_eq!(out.len(), 40 * FRAME * 2);
        assert!(rms(&out[30 * FRAME * 2..]) > 5000.0);
        // lose packet 20: FEC from packet 21 restores it, concealment alone also yields a non-silent frame
        let mut dec = Decoder::new().unwrap();
        let mut out = Vec::new();
        for (k, p) in packets.iter().enumerate() {
            if k == 20 {
                continue;
            }
            if k == 21 {
                dec.decode(Some(p), true, &mut out).unwrap();
            }
            dec.decode(Some(p), false, &mut out).unwrap();
        }
        assert_eq!(out.len(), 40 * FRAME * 2);
        let fec_frame = &out[20 * FRAME * 2..21 * FRAME * 2];
        assert!(rms(fec_frame) > 3000.0, "FEC frame rms {}", rms(fec_frame));
        let mut dec = Decoder::new().unwrap();
        let mut out = Vec::new();
        for (k, p) in packets.iter().enumerate() {
            if k == 20 {
                dec.decode(None, false, &mut out).unwrap();
            } else {
                dec.decode(Some(p), false, &mut out).unwrap();
            }
        }
        assert!(rms(&out[20 * FRAME * 2..21 * FRAME * 2]) > 3000.0, "PLC frame is not silence");
    }
}
