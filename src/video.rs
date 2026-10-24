// Thin wrappers over the FFmpeg C API: HEVC NVENC encoder, D3D11VA decoder, swscale converter.
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

pub fn set_log_quiet() {
    unsafe { av_log_set_level(AV_LOG_ERROR as i32) };
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    pub preset: String,
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
            let codec = avcodec_find_encoder_by_name(c"hevc_nvenc".as_ptr());
            if codec.is_null() {
                bail!("в FFmpeg нет hevc_nvenc");
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
            // Huge GOP: keyframes only at start and when the client asks after packet loss.
            c.gop_size = cfg.fps as i32 * 600;
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

unsafe extern "C" fn pick_d3d11(_ctx: *mut AVCodecContext, fmts: *const AVPixelFormat) -> AVPixelFormat {
    let mut p = fmts;
    let mut last = AVPixelFormat::AV_PIX_FMT_NONE;
    while *p != AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == AVPixelFormat::AV_PIX_FMT_D3D11 {
            return *p;
        }
        last = *p;
        p = p.add(1);
    }
    last // software fallback is listed last
}

pub struct Decoder {
    ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    frame: *mut AVFrame,
    sw: *mut AVFrame,
    pub hardware: bool,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(allow_hw: bool) -> Result<Decoder> {
        unsafe {
            let codec = avcodec_find_decoder(AVCodecID::AV_CODEC_ID_HEVC);
            if codec.is_null() {
                bail!("в FFmpeg нет декодера HEVC");
            }
            let ctx = avcodec_alloc_context3(codec);
            let mut hardware = false;
            if allow_hw {
                let mut dev: *mut AVBufferRef = ptr::null_mut();
                if av_hwdevice_ctx_create(&mut dev, AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, ptr::null(), ptr::null_mut(), 0) >= 0 {
                    (*ctx).hw_device_ctx = av_buffer_ref(dev);
                    av_buffer_unref(&mut dev);
                    (*ctx).get_format = Some(pick_d3d11);
                    hardware = true;
                }
            }
            (*ctx).flags |= AV_CODEC_FLAG_LOW_DELAY as i32;
            if hardware {
                (*ctx).thread_count = 1;
            } else {
                // Slice threads only: frame threads would add frames of latency.
                (*ctx).thread_count = 0;
                (*ctx).thread_type = FF_THREAD_SLICE as i32;
            }
            let r = avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                let mut ctx = ctx;
                avcodec_free_context(&mut ctx);
                bail!("avcodec_open2 (decoder): {}", av_err(r));
            }
            Ok(Decoder { ctx, pkt: av_packet_alloc(), frame: av_frame_alloc(), sw: av_frame_alloc(), hardware })
        }
    }

    /// Decodes one access unit and calls `out` with each software frame (NV12 or YUV420P).
    pub fn decode(&mut self, data: &[u8], mut out: impl FnMut(&AVFrame)) -> Result<()> {
        unsafe {
            check(av_new_packet(self.pkt, data.len() as i32), "av_new_packet")?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let r = avcodec_send_packet(self.ctx, self.pkt);
            av_packet_unref(self.pkt);
            check(r, "avcodec_send_packet")?;
            loop {
                let r = avcodec_receive_frame(self.ctx, self.frame);
                if r == EAGAIN || r == AVERROR_EOF {
                    break;
                }
                check(r, "avcodec_receive_frame")?;
                if (*self.frame).format == AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                    av_frame_unref(self.sw);
                    check(av_hwframe_transfer_data(self.sw, self.frame, 0), "av_hwframe_transfer_data")?;
                    (*self.sw).colorspace = (*self.frame).colorspace;
                    (*self.sw).color_range = (*self.frame).color_range;
                    out(&*self.sw);
                } else {
                    out(&*self.frame);
                }
                av_frame_unref(self.frame);
            }
        }
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.pkt);
            av_frame_free(&mut self.frame);
            av_frame_free(&mut self.sw);
            avcodec_free_context(&mut self.ctx);
        }
    }
}

/// YUV frame -> BGRA (as u32 0x00RRGGBB, what softbuffer wants) with scaling.
pub struct Scaler {
    sws: *mut SwsContext,
    key: (i32, i32, i32, u32, u32, i32, i32),
}

unsafe impl Send for Scaler {}

impl Scaler {
    pub fn new() -> Scaler {
        Scaler { sws: ptr::null_mut(), key: (0, 0, 0, 0, 0, 0, 0) }
    }

    pub fn convert(&mut self, f: &AVFrame, dw: u32, dh: u32, out: &mut Vec<u32>) -> Result<()> {
        unsafe {
            let key = (f.width, f.height, f.format, dw, dh, f.colorspace as i32, f.color_range as i32);
            if self.sws.is_null() || key != self.key {
                if !self.sws.is_null() {
                    sws_freeContext(self.sws);
                }
                let fmt: AVPixelFormat = std::mem::transmute(f.format);
                self.sws = sws_getContext(
                    f.width,
                    f.height,
                    fmt,
                    dw as i32,
                    dh as i32,
                    AVPixelFormat::AV_PIX_FMT_BGR0,
                    4, // SWS_BICUBIC (an enum in FFmpeg 8 bindings): keeps text crisp when downscaling
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                );
                if self.sws.is_null() {
                    bail!("sws_getContext");
                }
                let cs = if f.colorspace == AVColorSpace::AVCOL_SPC_BT470BG || f.colorspace == AVColorSpace::AVCOL_SPC_SMPTE170M {
                    SWS_CS_ITU601
                } else {
                    SWS_CS_ITU709
                };
                let src_full = (f.color_range == AVColorRange::AVCOL_RANGE_JPEG) as i32;
                let coeffs = sws_getCoefficients(cs as i32);
                sws_setColorspaceDetails(self.sws, coeffs, src_full, coeffs, 1, 0, 1 << 16, 1 << 16);
                self.key = key;
            }
            out.resize((dw * dh) as usize, 0);
            let dst = [out.as_mut_ptr() as *mut u8, ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let dst_stride = [(dw * 4) as i32, 0, 0, 0];
            sws_scale(
                self.sws,
                f.data.as_ptr() as *const *const u8,
                f.linesize.as_ptr(),
                0,
                f.height,
                dst.as_ptr(),
                dst_stride.as_ptr(),
            );
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
