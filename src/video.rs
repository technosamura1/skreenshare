// Thin wrappers over the FFmpeg C API: H.264 NVENC encoder (low latency) and a swscale BGRA resizer.
use anyhow::{bail, Result};
use ffmpeg_next::ffi::*;
use std::ffi::{CStr, CString};
use std::ptr;

const EAGAIN: i32 = -11;

fn av_err(code: i32) -> String {
    let mut buf = [0i8; 256];
    unsafe {
        av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

fn check(code: i32, what: &str) -> Result<i32> {
    if code < 0 {
        bail!("{what}: {}", av_err(code));
    }
    Ok(code)
}

/// Unpacks the bundled FFmpeg DLLs (first start only) and initialises FFmpeg. Safe to call from any thread, any number of times.
pub fn init() -> Result<()> {
    static INIT: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        crate::bundle::ensure_ffmpeg().map_err(|e| format!("не удалось подготовить FFmpeg: {e:#}"))?;
        ffmpeg_next::init().map_err(|e| format!("FFmpeg не загрузился: {e}"))?;
        unsafe { av_log_set_level(AV_LOG_ERROR as i32) };
        Ok(())
    })
    .clone()
    .map_err(|e| anyhow::anyhow!(e))
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    pub preset: String,
    pub gop: u32,
}

pub struct Encoder {
    ctx: *mut AVCodecContext,
    frame: *mut AVFrame,
    pkt: *mut AVPacket,
    pts: i64,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(cfg: &EncoderConfig) -> Result<Encoder> {
        unsafe {
            let codec = avcodec_find_encoder_by_name(c"h264_nvenc".as_ptr());
            if codec.is_null() {
                bail!("в FFmpeg нет h264_nvenc");
            }
            let ctx = avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.width = cfg.width as i32;
            c.height = cfg.height as i32;
            c.time_base = AVRational { num: 1, den: cfg.fps as i32 };
            c.framerate = AVRational { num: cfg.fps as i32, den: 1 };
            c.pix_fmt = AVPixelFormat::AV_PIX_FMT_BGRA;
            c.bit_rate = cfg.bitrate as i64;
            c.rc_max_rate = cfg.bitrate as i64;
            // ~2 frames of VBV: low latency without starving detailed frames.
            c.rc_buffer_size = (cfg.bitrate / cfg.fps as u64 * 2) as i32;
            // A keyframe every couple of seconds, plus on request after packet loss: a lost packet heals itself.
            c.gop_size = cfg.gop as i32;
            c.max_b_frames = 0;
            c.colorspace = AVColorSpace::AVCOL_SPC_BT709;
            c.color_primaries = AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            c.color_range = AVColorRange::AVCOL_RANGE_MPEG;

            let mut opts: *mut AVDictionary = ptr::null_mut();
            let preset = CString::new(cfg.preset.as_str()).unwrap();
            av_dict_set(&mut opts, c"preset".as_ptr(), preset.as_ptr(), 0);
            for (k, v) in [
                (c"tune", c"ull"),
                (c"profile", c"main"),
                (c"rc-lookahead", c"0"),
                (c"b_ref_mode", c"disabled"),
                (c"rc", c"cbr"),
                (c"zerolatency", c"1"),
                (c"delay", c"0"),
                (c"forced-idr", c"1"),
                (c"no-scenecut", c"1"),
            ] {
                av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            let r = avcodec_open2(ctx, codec, &mut opts);
            av_dict_free(&mut opts);
            if r < 0 {
                let mut ctx = ctx;
                avcodec_free_context(&mut ctx);
                bail!("не удалось запустить NVENC: {} (нужна видеокарта NVIDIA и свежий драйвер)", av_err(r));
            }

            let frame = av_frame_alloc();
            (*frame).format = AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*frame).width = cfg.width as i32;
            (*frame).height = cfg.height as i32;
            check(av_frame_get_buffer(frame, 64), "av_frame_get_buffer")?;
            Ok(Encoder { ctx, frame, pkt: av_packet_alloc(), pts: 0, width: cfg.width, height: cfg.height })
        }
    }

    /// The BGRA input buffer for the next frame and its row stride.
    pub fn input(&mut self) -> Result<(&mut [u8], usize)> {
        unsafe {
            check(av_frame_make_writable(self.frame), "av_frame_make_writable")?;
            let stride = (*self.frame).linesize[0] as usize;
            let buf = std::slice::from_raw_parts_mut((*self.frame).data[0], stride * self.height as usize);
            Ok((buf, stride))
        }
    }

    pub fn encode(&mut self, force_key: bool, mut out: impl FnMut(&[u8], bool)) -> Result<()> {
        unsafe {
            (*self.frame).pts = self.pts;
            self.pts += 1;
            (*self.frame).pict_type =
                if force_key { AVPictureType::AV_PICTURE_TYPE_I } else { AVPictureType::AV_PICTURE_TYPE_NONE };
            check(avcodec_send_frame(self.ctx, self.frame), "avcodec_send_frame")?;
            loop {
                let r = avcodec_receive_packet(self.ctx, self.pkt);
                if r == EAGAIN || r == AVERROR_EOF {
                    break;
                }
                check(r, "avcodec_receive_packet")?;
                let p = &*self.pkt;
                out(std::slice::from_raw_parts(p.data, p.size as usize), p.flags & AV_PKT_FLAG_KEY as i32 != 0);
                av_packet_unref(self.pkt);
            }
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.pkt);
            av_frame_free(&mut self.frame);
            avcodec_free_context(&mut self.ctx);
        }
    }
}

/// BGRA -> BGRA resize (used only when the desktop is larger than what the projector can show).
pub struct Scaler {
    sws: *mut SwsContext,
    key: (u32, u32, u32, u32),
}

unsafe impl Send for Scaler {}

impl Scaler {
    pub fn new() -> Scaler {
        Scaler { sws: ptr::null_mut(), key: (0, 0, 0, 0) }
    }

    /// `src`/`dst` are BGRA with the given row strides.
    pub fn scale(&mut self, src: &[u8], sw: u32, sh: u32, sstride: usize, dst: &mut [u8], dw: u32, dh: u32, dstride: usize) -> Result<()> {
        unsafe {
            let key = (sw, sh, dw, dh);
            if self.sws.is_null() || key != self.key {
                if !self.sws.is_null() {
                    sws_freeContext(self.sws);
                }
                self.sws = sws_getContext(
                    sw as i32,
                    sh as i32,
                    AVPixelFormat::AV_PIX_FMT_BGRA,
                    dw as i32,
                    dh as i32,
                    AVPixelFormat::AV_PIX_FMT_BGRA,
                    4, // SWS_BICUBIC: keeps text readable when downscaling
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                );
                if self.sws.is_null() {
                    bail!("sws_getContext");
                }
                self.key = key;
            }
            if src.len() < sstride * sh as usize || dst.len() < dstride * dh as usize {
                bail!("scaler: буфер слишком мал");
            }
            let s = [src.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let ss = [sstride as i32, 0, 0, 0];
            let d = [dst.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let ds = [dstride as i32, 0, 0, 0];
            sws_scale(self.sws, s.as_ptr(), ss.as_ptr(), 0, sh as i32, d.as_ptr(), ds.as_ptr());
        }
        Ok(())
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        if !self.sws.is_null() {
            unsafe { sws_freeContext(self.sws) };
        }
    }
}
