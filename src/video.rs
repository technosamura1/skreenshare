// Thin wrappers over the FFmpeg C API: HEVC NVENC encoder (x264 fallback), D3D11VA decoder, swscale converter.
use anyhow::{bail, Result};
use ffmpeg_next::ffi::*;
use crate::encsel::{self, Codec, SoftPlan};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Oldest NVIDIA driver (Windows) that has the NVENC API this build of FFmpeg was compiled against
/// (nv-codec-headers n11.1.5.3 = NVENC API 11.1). FFmpeg prints the same number itself when the driver is older.
pub const NVENC_MIN_DRIVER: &str = "471.41";

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

/// FFmpeg's own messages while an encoder is being opened (the reason NVENC refuses to start is only there).
static OPEN_LOG: Mutex<String> = Mutex::new(String::new());
static CAPTURING: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn log_cb(ptr: *mut c_void, level: c_int, fmt: *const c_char, vl: va_list) {
    let mut buf = [0 as c_char; 1024];
    let mut prefix: c_int = 1;
    av_log_format_line2(ptr, level, fmt, vl, buf.as_mut_ptr(), buf.len() as c_int, &mut prefix);
    let line = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
    if CAPTURING.load(Ordering::Relaxed) {
        if let Ok(mut l) = OPEN_LOG.lock() {
            if l.len() < 16_000 {
                l.push_str(&line);
            }
        }
    } else if level <= AV_LOG_ERROR as i32 {
        eprint!("{line}");
    }
}

pub fn set_log_quiet() {
    unsafe {
        av_log_set_callback(Some(log_cb));
        av_log_set_level(AV_LOG_ERROR as i32);
    }
}

/// Runs `f` with FFmpeg's log at verbose level and returns what it printed.
fn with_log<T>(f: impl FnOnce() -> T) -> (T, String) {
    if let Ok(mut l) = OPEN_LOG.lock() {
        l.clear();
    }
    CAPTURING.store(true, Ordering::Relaxed);
    unsafe { av_log_set_level(AV_LOG_VERBOSE as i32) };
    let r = f();
    unsafe { av_log_set_level(AV_LOG_ERROR as i32) };
    CAPTURING.store(false, Ordering::Relaxed);
    let log = OPEN_LOG.lock().map(|l| l.clone()).unwrap_or_default();
    (r, log)
}

/// NVENC failed once on this PC: later encoders (e.g. after a resolution change) go straight to the processor.
static NVENC_BAD: AtomicBool = AtomicBool::new(false);
static NVENC_NOTE: Mutex<String> = Mutex::new(String::new());

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    pub preset: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Nvenc,
    /// Windows Media Foundation hardware MFT (Intel Quick Sync, AMD, ...).
    Mf,
    X264,
}

pub struct Encoder {
    ctx: *mut AVCodecContext,
    /// BGRA frame the caller draws into (`input()`), always the size of the captured picture.
    frame: *mut AVFrame,
    /// Software path only: YUV 4:2:0 picture at the (possibly smaller) encoded size, and BGRA -> YUV converter.
    yuv: *mut AVFrame,
    sws: *mut SwsContext,
    pkt: *mut AVPacket,
    pts: i64,
    /// Frame rate the encoder runs at (software encoding may be limited to 30).
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub backend: Backend,
    #[allow(dead_code)]
    pub codec: Codec,
    /// Size of the encoded picture (differs from `width`x`height` when software encoding scales down).
    #[allow(dead_code)]
    pub out_w: u32,
    #[allow(dead_code)]
    pub out_h: u32,
    /// For the status line, e.g. "NVENC · HEVC · 2560x1440".
    pub label: String,
    /// Top bitrate worth using (software encoding), Mbit/s.
    pub max_mbps: Option<f64>,
}

unsafe impl Send for Encoder {}

fn set_common(c: &mut AVCodecContext, cfg: &EncoderConfig, fps: u32, bitrate: u64) {
    c.time_base = AVRational { num: 1, den: fps as i32 };
    c.framerate = AVRational { num: fps as i32, den: 1 };
    c.bit_rate = bitrate as i64;
    c.rc_max_rate = bitrate as i64;
    // ~2 frames of VBV: low latency without starving detailed frames.
    c.rc_buffer_size = (bitrate / fps as u64 * 2) as i32;
    // Huge GOP: keyframes only at start and when the client asks after packet loss.
    c.gop_size = fps as i32 * 600;
    c.max_b_frames = 0;
    c.colorspace = AVColorSpace::AVCOL_SPC_BT709;
    c.color_primaries = AVColorPrimaries::AVCOL_PRI_BT709;
    c.color_trc = AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    c.color_range = AVColorRange::AVCOL_RANGE_MPEG;
    let _ = cfg;
}

impl Encoder {
    /// NVENC only (selftest, `--encoder nvenc`): the error carries a readable explanation and the minimum driver.
    pub fn new(cfg: &EncoderConfig) -> Result<Encoder> {
        Self::new_nvenc(cfg).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// The normal path: NVENC if the driver allows it, else x264 on the processor at a size the CPU can carry.
    /// `SS_ENCODER=nvenc|x264` forces one (testing). Returns the encoder and, when NVENC was skipped or failed,
    /// the reason (shown in the GUI as a notice).
    pub fn open_best(cfg: &EncoderConfig, cores: usize) -> Result<(Encoder, Option<String>)> {
        let forced = std::env::var("SS_ENCODER").unwrap_or_default().to_lowercase();
        let mut note = None;
        let mut nv_err = String::new();
        if forced != "x264" && forced != "mf" && !NVENC_BAD.load(Ordering::Relaxed) {
            match Self::new_nvenc(cfg) {
                Ok(e) => return Ok((e, None)),
                Err(why) => {
                    NVENC_BAD.store(true, Ordering::Relaxed);
                    if let Ok(mut n) = NVENC_NOTE.lock() {
                        *n = why.clone();
                    }
                    nv_err = why;
                }
            }
        } else if forced == "x264" || forced == "mf" {
            nv_err = format!("NVENC отключён (SS_ENCODER={forced})");
        } else if let Ok(n) = NVENC_NOTE.lock() {
            nv_err = n.clone();
        }
        if forced == "nvenc" {
            bail!("{nv_err}");
        }
        if !nv_err.is_empty() {
            note = Some(nv_err.clone());
        }
        // No NVIDIA (or its driver refused): the graphics chip's own encoder through Media Foundation.
        if forced != "x264" {
            match Self::new_mf(cfg) {
                Ok(e) => return Ok((e, None)), // works: the missing NVIDIA is no news,
                Err(e) if forced == "mf" => bail!("Media Foundation: {e:#}"),
                Err(e) => eprintln!("Media Foundation: {e:#}"),
            }
        }
        let plan = encsel::soft_plan(cores, cfg.width, cfg.height, cfg.fps);
        match Self::new_x264(cfg, plan) {
            Ok(e) => Ok((e, note)),
            Err(x) => bail!(
                "Не удалось запустить кодирование видео. {nv_err} Кодирование на процессоре (x264) тоже не запустилось: {x:#}. \
                 Что сделать: 1) обновите драйвер видеокарты NVIDIA (нужен {NVENC_MIN_DRIVER} или новее) на nvidia.com/drivers и перезапустите программу; \
                 2) закройте другие программы, которые могут занимать кодировщик (запись экрана, OBS, GeForce Experience); \
                 3) если не помогло — пришлите файл viewer.log или этот текст разработчику."
            ),
        }
    }

    fn alloc_input(cfg: &EncoderConfig) -> Result<*mut AVFrame> {
        unsafe {
            let frame = av_frame_alloc();
            (*frame).format = AVPixelFormat::AV_PIX_FMT_BGRA as i32;
            (*frame).width = cfg.width as i32;
            (*frame).height = cfg.height as i32;
            check(av_frame_get_buffer(frame, 64), "av_frame_get_buffer")?;
            Ok(frame)
        }
    }

    fn new_nvenc(cfg: &EncoderConfig) -> std::result::Result<Encoder, String> {
        unsafe {
            let codec = avcodec_find_encoder_by_name(c"hevc_nvenc".as_ptr());
            if codec.is_null() {
                return Err("в FFmpeg нет hevc_nvenc".into());
            }
            let ctx = avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.width = cfg.width as i32;
            c.height = cfg.height as i32;
            c.pix_fmt = AVPixelFormat::AV_PIX_FMT_BGRA;
            set_common(c, cfg, cfg.fps, cfg.bitrate);

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
            let (r, log) = with_log(|| avcodec_open2(ctx, codec, &mut opts));
            av_dict_free(&mut opts);
            if r < 0 {
                let mut ctx = ctx;
                avcodec_free_context(&mut ctx);
                let why = encsel::explain_nvenc(&av_err(r), &log, NVENC_MIN_DRIVER);
                let tail = encsel::parse_nv_log(&log).tail.join(" | ");
                eprintln!("NVENC: {} ({}) FFmpeg: {tail}", av_err(r), r);
                return Err(why);
            }
            let gpu = encsel::parse_nv_log(&log).gpus.first().cloned();
            let frame = Self::alloc_input(cfg).map_err(|e| format!("{e:#}"))?;
            Ok(Encoder {
                ctx,
                frame,
                yuv: ptr::null_mut(),
                sws: ptr::null_mut(),
                pkt: av_packet_alloc(),
                pts: 0,
                fps: cfg.fps,
                width: cfg.width,
                height: cfg.height,
                backend: Backend::Nvenc,
                codec: Codec::Hevc,
                out_w: cfg.width,
                out_h: cfg.height,
                label: format!(
                    "Кодирование: видеокарта NVENC (HEVC){} · {}x{}",
                    gpu.map(|g| format!(", {g}")).unwrap_or_default(),
                    cfg.width,
                    cfg.height
                ),
                max_mbps: None,
            })
        }
    }

    fn new_x264(cfg: &EncoderConfig, plan: SoftPlan) -> Result<Encoder> {
        let bitrate = ((cfg.bitrate as f64 / 1e6).min(plan.max_mbps) * 1e6) as u64;
        let opts = [
            (c"preset", c"ultrafast"),
            (c"tune", c"zerolatency"),
            (c"forced-idr", c"1"),
            (c"x264-params", c"scenecut=0:repeat-headers=1:aud=0"),
        ];
        let label = format!("Кодирование: процессор x264 (H.264) · {}x{} · {} кадров/с", plan.width, plan.height, plan.fps);
        Self::new_converted(cfg, plan, c"libx264", AVPixelFormat::AV_PIX_FMT_YUV420P, bitrate, &opts, 0, Backend::X264, label)
    }

    /// The video block of any graphics chip through Windows Media Foundation (Intel Quick Sync on laptops
    /// without NVIDIA, AMD, or NVIDIA's own MFT): hardware H.264, the processor only converts colours.
    fn new_mf(cfg: &EncoderConfig) -> Result<Encoder> {
        let plan = encsel::mf_plan(cfg.width, cfg.height, cfg.fps);
        let bitrate = cfg.bitrate.min((plan.max_mbps * 1e6) as u64);
        let opts = [(c"hw_encoding", c"1"), (c"rate_control", c"cbr"), (c"scenario", c"display_remoting")];
        let label = format!("Кодирование: видеоядро через Media Foundation (H.264) · {}x{} · {} кадров/с", plan.width, plan.height, plan.fps);
        Self::new_converted(cfg, plan, c"h264_mf", AVPixelFormat::AV_PIX_FMT_NV12, bitrate, &opts, AV_CODEC_FLAG_LOW_DELAY as i32, Backend::Mf, label)
    }

    /// An H.264 encoder fed from the BGRA input through swscale (colour conversion + scaling to `plan`).
    #[allow(clippy::too_many_arguments)]
    fn new_converted(
        cfg: &EncoderConfig,
        plan: SoftPlan,
        name: &std::ffi::CStr,
        pix: AVPixelFormat,
        bitrate: u64,
        options: &[(&std::ffi::CStr, &std::ffi::CStr)],
        flags: i32,
        backend: Backend,
        label: String,
    ) -> Result<Encoder> {
        unsafe {
            let codec = avcodec_find_encoder_by_name(name.as_ptr());
            if codec.is_null() {
                bail!("в FFmpeg нет {}", name.to_string_lossy());
            }
            let ctx = avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.width = plan.width as i32;
            c.height = plan.height as i32;
            c.pix_fmt = pix;
            c.flags |= flags;
            set_common(c, cfg, plan.fps, bitrate);
            let mut opts: *mut AVDictionary = ptr::null_mut();
            for (k, v) in options {
                av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            let (r, log) = with_log(|| avcodec_open2(ctx, codec, &mut opts));
            av_dict_free(&mut opts);
            if r < 0 {
                let mut ctx = ctx;
                avcodec_free_context(&mut ctx);
                let tail: Vec<&str> = log.lines().rev().take(3).collect();
                bail!("{} ({})", av_err(r), tail.join(" | "));
            }
            let frame = Self::alloc_input(cfg)?;
            let yuv = av_frame_alloc();
            (*yuv).format = pix as i32;
            (*yuv).width = plan.width as i32;
            (*yuv).height = plan.height as i32;
            check(av_frame_get_buffer(yuv, 32), "av_frame_get_buffer")?;
            let sws = sws_getContext(
                cfg.width as i32,
                cfg.height as i32,
                AVPixelFormat::AV_PIX_FMT_BGRA,
                plan.width as i32,
                plan.height as i32,
                pix,
                2, // SWS_BILINEAR: cheap, no ringing on text when scaling down
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if sws.is_null() {
                bail!("sws_getContext");
            }
            let coeffs = sws_getCoefficients(SWS_CS_ITU709 as i32);
            sws_setColorspaceDetails(sws, coeffs, 1, coeffs, 0, 0, 1 << 16, 1 << 16);
            Ok(Encoder {
                ctx,
                frame,
                yuv,
                sws,
                pkt: av_packet_alloc(),
                pts: 0,
                fps: plan.fps,
                width: cfg.width,
                height: cfg.height,
                backend,
                codec: Codec::H264,
                out_w: plan.width,
                out_h: plan.height,
                label,
                max_mbps: Some(plan.max_mbps),
            })
        }
    }

    /// Changes the target bitrate on the fly; FFmpeg's NVENC wrapper notices the new value on the next
    /// frame and reconfigures the encoder without a keyframe (GPUs with dynamic bitrate support).
    pub fn set_bitrate(&mut self, bitrate: u64) {
        unsafe {
            let c = &mut *self.ctx;
            c.bit_rate = bitrate as i64;
            c.rc_max_rate = bitrate as i64;
            c.rc_buffer_size = (bitrate / self.fps as u64 * 2) as i32;
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

    /// The BGRA frame given to the encoder last time (and its row stride), read-only and without touching the
    /// frame: valid until the next `input()`. Only meaningful after a frame has been encoded.
    pub fn last_input(&self) -> (&[u8], usize) {
        unsafe {
            let stride = (*self.frame).linesize[0] as usize;
            (std::slice::from_raw_parts((*self.frame).data[0], stride * self.height as usize), stride)
        }
    }

    pub fn encode(&mut self, force_key: bool, mut out: impl FnMut(&[u8], bool)) -> Result<()> {
        unsafe {
            let send = if !self.yuv.is_null() {
                check(av_frame_make_writable(self.yuv), "av_frame_make_writable")?;
                let s = [(*self.frame).data[0] as *const u8, ptr::null(), ptr::null(), ptr::null()];
                let ss = [(*self.frame).linesize[0], 0, 0, 0];
                sws_scale(
                    self.sws,
                    s.as_ptr(),
                    ss.as_ptr(),
                    0,
                    self.height as i32,
                    (*self.yuv).data.as_ptr() as *const *mut u8,
                    (*self.yuv).linesize.as_ptr(),
                );
                self.yuv
            } else {
                self.frame
            };
            (*send).pts = self.pts;
            self.pts += 1;
            (*send).pict_type =
                if force_key { AVPictureType::AV_PICTURE_TYPE_I } else { AVPictureType::AV_PICTURE_TYPE_NONE };
            check(avcodec_send_frame(self.ctx, send), "avcodec_send_frame")?;
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
            if !self.yuv.is_null() {
                av_frame_free(&mut self.yuv);
            }
            if !self.sws.is_null() {
                sws_freeContext(self.sws);
            }
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
    /// Hardware decoding was set up (the GPU device exists).
    pub hardware: bool,
    /// The last picture really came out of the GPU decoder. FFmpeg quietly falls back to its software decoder
    /// when the GPU can't do this stream; then this stays false although `hardware` is true.
    pub hw_active: bool,
    /// Hand out the decoder's own GPU frames (format D3D11, no readback) instead of NV12 copies in memory; the
    /// caller then converts them on the GPU (gpuconv.rs) or calls `readback` itself. The frame is only valid
    /// inside the callback.
    pub hw_passthrough: bool,
    pub codec: Codec,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(allow_hw: bool, kind: Codec) -> Result<Decoder> {
        unsafe {
            let id = match kind {
                Codec::Hevc => AVCodecID::AV_CODEC_ID_HEVC,
                Codec::H264 => AVCodecID::AV_CODEC_ID_H264,
            };
            let codec = avcodec_find_decoder(id);
            if codec.is_null() {
                bail!("в FFmpeg нет декодера {kind:?}");
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
            Ok(Decoder { ctx, pkt: av_packet_alloc(), frame: av_frame_alloc(), sw: av_frame_alloc(), hardware, hw_active: false, hw_passthrough: false, codec: kind })
        }
    }

    /// Decodes one access unit and calls `out` with each software frame (NV12 or YUV420P). With `show = false`
    /// the picture is decoded (later frames depend on it) but not read back from the GPU or handed out: catch-up
    /// after falling behind skips the expensive part this way. Returns how many pictures the packet produced.
    pub fn decode(&mut self, data: &[u8], show: bool, mut out: impl FnMut(&AVFrame)) -> Result<u32> {
        let mut pictures = 0;
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
                pictures += 1;
                let is_hw = (*self.frame).format == AVPixelFormat::AV_PIX_FMT_D3D11 as i32;
                self.hw_active = is_hw;
                if !show {
                    // nothing to do: dropping the reference below is enough
                } else if is_hw && self.hw_passthrough {
                    out(&*self.frame);
                } else if is_hw {
                    av_frame_unref(self.sw);
                    let t = av_hwframe_transfer_data(self.sw, self.frame, 0);
                    if t < 0 {
                        av_frame_unref(self.frame);
                        check(t, "av_hwframe_transfer_data")?;
                    }
                    (*self.sw).colorspace = (*self.frame).colorspace;
                    (*self.sw).color_range = (*self.frame).color_range;
                    out(&*self.sw);
                } else {
                    out(&*self.frame);
                }
                av_frame_unref(self.frame);
            }
        }
        Ok(pictures)
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
    key: (i32, i32, i32, u32, u32, i32, i32, i32),
    /// Cheaper filter (bilinear) instead of bicubic; set when conversion gets too slow.
    pub fast: bool,
}

unsafe impl Send for Scaler {}

impl Scaler {
    pub fn new() -> Scaler {
        Scaler { sws: ptr::null_mut(), key: (0, 0, 0, 0, 0, 0, 0, 0), fast: false }
    }

    pub fn convert(&mut self, f: &AVFrame, dw: u32, dh: u32, out: &mut Vec<u32>) -> Result<()> {
        unsafe {
            let flags = if self.fast { 2 } else { 4 }; // SWS_BILINEAR / SWS_BICUBIC (enums in the FFmpeg 8 bindings)
            let key = (f.width, f.height, f.format, dw, dh, f.colorspace as i32, f.color_range as i32, flags);
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
                    flags, // bicubic keeps text crisp when downscaling; bilinear is the cheap fallback
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

/// BGRA -> BGRA resize (a captured window is scaled into its place in the fixed stream canvas).
pub struct BgraScaler {
    sws: *mut SwsContext,
    key: (u32, u32, u32, u32),
    flags: i32,
}

unsafe impl Send for BgraScaler {}

impl BgraScaler {
    pub fn new() -> BgraScaler {
        BgraScaler { sws: ptr::null_mut(), key: (0, 0, 0, 0), flags: 4 }
    }

    /// For big downscales (the self-preview): SWS_AREA averages the covered pixels, quick and without aliasing.
    pub fn for_preview() -> BgraScaler {
        BgraScaler { sws: ptr::null_mut(), key: (0, 0, 0, 0), flags: 0x20 }
    }

    /// `src`/`dst` are BGRA with the given row strides.
    pub fn scale(&mut self, src: &[u8], sw: u32, sh: u32, sstride: usize, dst: &mut [u8], dw: u32, dh: u32, dstride: usize) -> Result<()> {
        unsafe {
            let key = (sw, sh, dw, dh);
            if self.sws.is_null() || key != self.key {
                if !self.sws.is_null() {
                    sws_freeContext(self.sws);
                    self.sws = ptr::null_mut();
                }
                self.sws = sws_getContext(
                    sw as i32,
                    sh as i32,
                    AVPixelFormat::AV_PIX_FMT_BGRA,
                    dw as i32,
                    dh as i32,
                    AVPixelFormat::AV_PIX_FMT_BGRA,
                    self.flags, // SWS_BICUBIC (4) keeps text readable when scaling; SWS_AREA for previews
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

impl Drop for BgraScaler {
    fn drop(&mut self) {
        if !self.sws.is_null() {
            unsafe { sws_freeContext(self.sws) };
        }
    }
}

/// A decoded picture copied out of the decoder's reusable frame, so it can be converted on another thread
/// while the decoder moves on to the next packet.
pub struct OwnedFrame(*mut AVFrame);

unsafe impl Send for OwnedFrame {}

impl OwnedFrame {
    /// Copies the pixels and the colour properties (~1 ms for 1080p NV12).
    pub fn copy_of(f: &AVFrame) -> Result<OwnedFrame> {
        unsafe {
            let n = av_frame_alloc();
            if n.is_null() {
                bail!("av_frame_alloc");
            }
            let o = OwnedFrame(n);
            (*n).format = f.format;
            (*n).width = f.width;
            (*n).height = f.height;
            if av_frame_get_buffer(n, 32) < 0 || av_frame_copy(n, f) < 0 || av_frame_copy_props(n, f) < 0 {
                bail!("копия кадра не удалась");
            }
            Ok(o)
        }
    }

    pub fn get(&self) -> &AVFrame {
        unsafe { &*self.0 }
    }
}

impl Drop for OwnedFrame {
    fn drop(&mut self) {
        unsafe { av_frame_free(&mut self.0) };
    }
}

/// Reads a GPU frame (format D3D11) back into a normal NV12/P010 frame in memory, colour properties included.
pub fn readback(f: &AVFrame) -> Result<OwnedFrame> {
    unsafe {
        let n = av_frame_alloc();
        if n.is_null() {
            bail!("av_frame_alloc");
        }
        let o = OwnedFrame(n);
        check(av_hwframe_transfer_data(n, f, 0), "av_hwframe_transfer_data")?;
        (*n).colorspace = f.colorspace;
        (*n).color_range = f.color_range;
        Ok(o)
    }
}

/// `convbench`: how long the viewer's NV12 -> window-size BGR0 conversion takes on this machine.
pub fn convert_bench(vw: i32, vh: i32, dw: u32, dh: u32) -> Result<()> {
    unsafe {
        let mut f = av_frame_alloc();
        (*f).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*f).width = vw;
        (*f).height = vh;
        if av_frame_get_buffer(f, 32) < 0 {
            bail!("av_frame_get_buffer");
        }
        for p in 0..2 {
            let n = (*f).linesize[p] as usize * if p == 0 { vh } else { vh / 2 } as usize;
            for i in 0..n {
                *(*f).data[p].add(i) = (i.wrapping_mul(31) >> 3) as u8;
            }
        }
        (*f).colorspace = AVColorSpace::AVCOL_SPC_BT709;
        (*f).color_range = AVColorRange::AVCOL_RANGE_MPEG;
        let mut out = Vec::new();
        for (name, fast) in [("бикубик", false), ("билинейный", true)] {
            let mut sc = Scaler::new();
            sc.fast = fast;
            sc.convert(&*f, dw, dh, &mut out)?; // warm-up, builds the context
            let t = std::time::Instant::now();
            let n = 20;
            for _ in 0..n {
                sc.convert(&*f, dw, dh, &mut out)?;
            }
            println!("{vw}x{vh} -> {dw}x{dh}, {name}: {:.1} мс/кадр", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
        av_frame_free(&mut f);
    }
    Ok(())
}
