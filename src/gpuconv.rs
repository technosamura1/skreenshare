// Viewer-side GPU colour conversion and scaling: the D3D11VA decoder's NV12/P010 texture goes straight into the
// D3D11 video processor (VideoProcessorBlt), which writes a BGRA texture of the on-screen size. That texture is
// copied to a staging texture and read into the same `Vec<u32>` the CPU path fills, so presenting (softbuffer,
// window and fullscreen mode) is unchanged. Any failure is reported to the caller, which switches to the CPU
// path for good (see gpulogic.rs). The CPU readback of the full-size decoded frame (~2 ms) and swscale
// (20+ ms at 4K -> 1080p window) are skipped while this works.
//
// All calls happen on the decoder thread, the same thread FFmpeg uses the D3D11 device from, so the immediate
// context is never used from two threads at once.
use crate::gpulogic;
use crate::video::{self, Scaler};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg_next::ffi::{av_frame_alloc, av_frame_free, av_frame_get_buffer, av_frame_unref, av_hwdevice_ctx_create, av_hwframe_ctx_alloc, av_hwframe_ctx_init, av_hwframe_get_buffer, av_hwframe_transfer_data, AVBufferRef, AVColorSpace, AVColorRange, AVFrame, AVHWFramesContext, AVHWDeviceType, AVPixelFormat};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// First fields of FFmpeg's AVD3D11VADeviceContext (hwcontext_d3d11va.h); the rest is not needed.
#[repr(C)]
struct D3d11vaDeviceCtx {
    device: *mut c_void,
    device_context: *mut c_void,
}

struct Cfg {
    key: (u32, u32, u32, u32, i32),
    venum: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    out_tex: ID3D11Texture2D,
    out_view: ID3D11VideoProcessorOutputView,
    staging: ID3D11Texture2D,
}

pub struct GpuConv {
    device_ptr: *mut c_void,
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    cfg: Option<Cfg>,
    rebuilt: bool,
}

unsafe fn device_ptr_of(f: &AVFrame) -> Result<*mut D3d11vaDeviceCtx> {
    if f.hw_frames_ctx.is_null() {
        bail!("у кадра нет hw_frames_ctx");
    }
    let frames = (*f.hw_frames_ctx).data as *mut AVHWFramesContext;
    if frames.is_null() || (*frames).device_ctx.is_null() {
        bail!("нет контекста устройства");
    }
    let dc = (*frames).device_ctx;
    if (*dc).type_ != AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA || (*dc).hwctx.is_null() {
        bail!("устройство не D3D11VA");
    }
    Ok((*dc).hwctx as *mut D3d11vaDeviceCtx)
}

impl GpuConv {
    pub fn new(f: &AVFrame) -> Result<GpuConv> {
        unsafe {
            let hw = device_ptr_of(f)?;
            let (dp, cp) = ((*hw).device, (*hw).device_context);
            let dev = ID3D11Device::from_raw_borrowed(&dp).ok_or_else(|| anyhow!("нет ID3D11Device"))?.clone();
            let ctx = ID3D11DeviceContext::from_raw_borrowed(&cp).ok_or_else(|| anyhow!("нет ID3D11DeviceContext"))?.clone();
            let vdev: ID3D11VideoDevice = dev.cast().context("у устройства нет ID3D11VideoDevice")?;
            let vctx: ID3D11VideoContext = ctx.cast().context("у контекста нет ID3D11VideoContext")?;
            Ok(GpuConv { device_ptr: dp, dev, ctx, vdev, vctx, cfg: None, rebuilt: false })
        }
    }

    /// The frame comes from the device these objects were made on.
    pub fn same_device(&self, f: &AVFrame) -> bool {
        unsafe { device_ptr_of(f).is_ok_and(|hw| (*hw).device == self.device_ptr) }
    }

    /// True once after the processor and textures were (re)built.
    pub fn take_rebuilt(&mut self) -> bool {
        std::mem::take(&mut self.rebuilt)
    }

    unsafe fn build(&mut self, fw: u32, fh: u32, dw: u32, dh: u32, fmt: DXGI_FORMAT) -> Result<()> {
        if dw == 0 || dh == 0 || dw > 16384 || dh > 16384 {
            bail!("странный размер вывода {dw}x{dh}");
        }
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: fw,
            InputHeight: fh,
            OutputFrameRate: rate,
            OutputWidth: dw,
            OutputHeight: dh,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let venum = self.vdev.CreateVideoProcessorEnumerator(&desc).context("CreateVideoProcessorEnumerator")?;
        let fin = venum.CheckVideoProcessorFormat(fmt).context("CheckVideoProcessorFormatSupport(вход)")?;
        if fin & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
            bail!("видеопроцессор не принимает формат кадров ({})", fmt.0);
        }
        let fout = venum.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM).context("CheckVideoProcessorFormatSupport(выход)")?;
        if fout & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            bail!("видеопроцессор не умеет выводить BGRA");
        }
        let vp = self.vdev.CreateVideoProcessor(&venum, 0).context("CreateVideoProcessor")?;

        let mut td = D3D11_TEXTURE2D_DESC {
            Width: dw,
            Height: dh,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut out_tex = None;
        self.dev.CreateTexture2D(&td, None, Some(&mut out_tex)).context("CreateTexture2D(выход)")?;
        let out_tex = out_tex.ok_or_else(|| anyhow!("CreateTexture2D: пусто"))?;
        td.Usage = D3D11_USAGE_STAGING;
        td.BindFlags = 0;
        td.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        self.dev.CreateTexture2D(&td, None, Some(&mut staging)).context("CreateTexture2D(staging)")?;
        let staging = staging.ok_or_else(|| anyhow!("CreateTexture2D(staging): пусто"))?;

        let mut od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC::default();
        od.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
        od.Anonymous.Texture2D = D3D11_TEX2D_VPOV { MipSlice: 0 };
        let mut out_view = None;
        self.vdev.CreateVideoProcessorOutputView(&out_tex, &venum, &od, Some(&mut out_view)).context("CreateVideoProcessorOutputView")?;
        let out_view = out_view.ok_or_else(|| anyhow!("CreateVideoProcessorOutputView: пусто"))?;

        self.cfg = Some(Cfg { key: (fw, fh, dw, dh, fmt.0), venum, vp, out_tex, out_view, staging });
        self.rebuilt = true;
        Ok(())
    }

    /// Hardware frame (format D3D11) -> `px` (dw*dh, 0x00RRGGBB). Blocks until the GPU is done with the frame,
    /// so the decoder may reuse its texture as soon as this returns.
    pub fn convert(&mut self, f: &AVFrame, dw: u32, dh: u32, px: &mut Vec<u32>) -> Result<()> {
        unsafe {
            if f.format != AVPixelFormat::AV_PIX_FMT_D3D11 as i32 || f.data[0].is_null() {
                bail!("кадр не в памяти видеокарты");
            }
            let (fw, fh) = (f.width as u32, f.height as u32);
            let tp = f.data[0] as *mut c_void;
            let slice = f.data[1] as usize as u32;
            let tex = ID3D11Texture2D::from_raw_borrowed(&tp).ok_or_else(|| anyhow!("нет текстуры кадра"))?.clone();
            let mut tdesc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut tdesc);
            let key = (fw, fh, dw, dh, tdesc.Format.0);
            if self.cfg.as_ref().is_none_or(|c| c.key != key) {
                self.cfg = None;
                self.build(fw, fh, dw, dh, tdesc.Format)?;
            }
            let cfg = self.cfg.as_ref().unwrap();

            let mut id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC::default();
            id.FourCC = 0;
            id.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
            id.Anonymous.Texture2D = D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice };
            let mut view = None;
            self.vdev.CreateVideoProcessorInputView(&tex, &cfg.venum, &id, Some(&mut view)).context("CreateVideoProcessorInputView")?;
            let view = view.ok_or_else(|| anyhow!("CreateVideoProcessorInputView: пусто"))?;

            let src = RECT { left: 0, top: 0, right: fw as i32, bottom: fh as i32 };
            let dst = RECT { left: 0, top: 0, right: dw as i32, bottom: dh as i32 };
            let bt601 = gpulogic::is_bt601(f.colorspace as i32);
            let full = f.color_range == AVColorRange::AVCOL_RANGE_JPEG;
            let cs_in = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: gpulogic::input_color_bits(bt601, full) };
            let cs_out = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: gpulogic::OUTPUT_COLOR_BITS };
            let vc = &self.vctx;
            vc.VideoProcessorSetStreamFrameFormat(&cfg.vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vc.VideoProcessorSetStreamColorSpace(&cfg.vp, 0, &cs_in);
            vc.VideoProcessorSetOutputColorSpace(&cfg.vp, &cs_out);
            vc.VideoProcessorSetStreamSourceRect(&cfg.vp, 0, true, Some(&src));
            vc.VideoProcessorSetStreamDestRect(&cfg.vp, 0, true, Some(&dst));
            vc.VideoProcessorSetOutputTargetRect(&cfg.vp, true, Some(&dst));
            vc.VideoProcessorSetStreamAutoProcessingMode(&cfg.vp, 0, false); // no driver "enhancements"

            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM::default();
            stream.Enable = true.into();
            stream.pInputSurface = ManuallyDrop::new(Some(view));
            let blt = vc.VideoProcessorBlt(&cfg.vp, &cfg.out_view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            blt.context("VideoProcessorBlt")?;

            self.ctx.CopyResource(&cfg.staging, &cfg.out_tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(&cfg.staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).context("Map(staging)")?;
            let (w, h) = (dw as usize, dh as usize);
            px.resize(w * h, 0);
            if m.pData.is_null() || (m.RowPitch as usize) < w * 4 {
                self.ctx.Unmap(&cfg.staging, 0);
                bail!("Map: неверный указатель/шаг");
            }
            for y in 0..h {
                let s = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u32, w);
                let d = &mut px[y * w..(y + 1) * w];
                for (o, i) in d.iter_mut().zip(s) {
                    *o = *i & 0x00FF_FFFF; // BGRA -> 0x00RRGGBB, alpha dropped like the CPU path
                }
            }
            self.ctx.Unmap(&cfg.staging, 0);
        }
        Ok(())
    }
}

/// One picture through the GPU path, including the guard's checks. Returns the GPU-vs-CPU difference when this
/// picture was cross-checked. If the check fails, `px` already holds the CPU picture and the guard is off, so
/// the caller can just show it. Errors leave the guard to the caller (it calls `fail`).
pub fn step(
    gpu: &mut Option<GpuConv>,
    guard: &mut gpulogic::GpuGuard,
    cpu: &mut Scaler,
    f: &AVFrame,
    dw: u32,
    dh: u32,
    px: &mut Vec<u32>,
) -> Result<Option<f64>> {
    if gpu.as_ref().is_none_or(|g| !g.same_device(f)) {
        *gpu = Some(GpuConv::new(f)?);
        guard.rebuilt();
    }
    let g = gpu.as_mut().unwrap();
    g.convert(f, dw, dh, px)?;
    if g.take_rebuilt() {
        guard.rebuilt();
    }
    if !guard.needs_verify() {
        return Ok(None);
    }
    let owned = video::readback(f)?;
    let mut reference = Vec::new();
    cpu.convert(owned.get(), dw, dh, &mut reference)?;
    let diff = gpulogic::mean_abs_diff(px, &reference, dw as usize, dh as usize);
    if guard.verified(diff).is_err() {
        std::mem::swap(px, &mut reference);
    }
    Ok(Some(diff))
}

/// `screenshare gputest`: checks the GPU path without any network or host. Builds a synthetic NV12 picture
/// (colour bars + gradient), uploads it into a D3D11VA texture like the decoder's, runs it through the video
/// processor and through the CPU path, and compares the two. Prints the speed of both. Safe to run anywhere:
/// if the video card can't do it, it says so (the viewer would then use the CPU path).
pub fn selftest(vw: i32, vh: i32, dw: u32, dh: u32) -> Result<()> {
    use std::time::Instant;
    unsafe {
        // 1. synthetic NV12 picture in memory
        let mut sw = av_frame_alloc();
        (*sw).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*sw).width = vw;
        (*sw).height = vh;
        if av_frame_get_buffer(sw, 32) < 0 {
            bail!("av_frame_get_buffer");
        }
        // BT.709 limited-range colour bars on top, a gradient below
        let bars: [(u8, u8, u8); 8] = [(235, 128, 128), (219, 16, 138), (188, 154, 16), (173, 42, 26), (78, 214, 230), (63, 102, 240), (32, 240, 118), (16, 128, 128)];
        for y in 0..vh as usize {
            for x in 0..vw as usize {
                let (yy, _, _) = if y < vh as usize / 2 { bars[x * 8 / vw as usize] } else { ((16 + x * 219 / vw as usize) as u8, 128, 128) };
                *(*sw).data[0].add(y * (*sw).linesize[0] as usize + x) = yy;
            }
        }
        for y in 0..vh as usize / 2 {
            for x in 0..vw as usize / 2 {
                let (_, u, v) = if y < vh as usize / 4 { bars[x * 8 / (vw as usize / 2)] } else { (0, 128, 128) };
                let o = y * (*sw).linesize[1] as usize + x * 2;
                *(*sw).data[1].add(o) = u;
                *(*sw).data[1].add(o + 1) = v;
            }
        }
        (*sw).colorspace = AVColorSpace::AVCOL_SPC_BT709;
        (*sw).color_range = AVColorRange::AVCOL_RANGE_MPEG;

        // 2. D3D11VA device and a hardware frame like the decoder produces
        let mut dev: *mut AVBufferRef = std::ptr::null_mut();
        let r = av_hwdevice_ctx_create(&mut dev, AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, std::ptr::null(), std::ptr::null_mut(), 0);
        if r < 0 {
            bail!("D3D11VA-устройство не создалось (код {r}): на этом ПК видеокарта не отдаёт D3D11 — зритель будет работать на процессоре");
        }
        let fctx = av_hwframe_ctx_alloc(dev);
        let fc = (*fctx).data as *mut AVHWFramesContext;
        (*fc).format = AVPixelFormat::AV_PIX_FMT_D3D11;
        (*fc).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
        (*fc).width = vw;
        (*fc).height = vh;
        (*fc).initial_pool_size = 4;
        let r = av_hwframe_ctx_init(fctx);
        if r < 0 {
            bail!("av_hwframe_ctx_init: код {r}");
        }
        let mut hwf = av_frame_alloc();
        if av_hwframe_get_buffer(fctx, hwf, 0) < 0 {
            bail!("av_hwframe_get_buffer");
        }
        if av_hwframe_transfer_data(hwf, sw, 0) < 0 {
            bail!("загрузка кадра в текстуру не удалась");
        }
        (*hwf).width = vw;
        (*hwf).height = vh;
        (*hwf).colorspace = (*sw).colorspace;
        (*hwf).color_range = (*sw).color_range;

        // 3. the GPU path (through `step`, i.e. with the same cross-check the viewer does) and the CPU path
        let mut cpu = Scaler::new();
        let mut cpx = Vec::new();
        cpu.convert(&*sw, dw, dh, &mut cpx)?;
        let mut guard = gpulogic::GpuGuard::new(false, true);
        let mut gpu: Option<GpuConv> = None;
        let mut gpx = Vec::new();
        let first = step(&mut gpu, &mut guard, &mut cpu, &*hwf, dw, dh, &mut gpx);
        let failure = match (&first, guard.active()) {
            (Err(e), _) => Some(format!("{e:#}")),
            (Ok(_), false) => Some(guard.reason().unwrap_or("?").to_string()),
            (Ok(_), true) => None,
        };
        if let Some(why) = failure {
            println!("ВИДЕОКАРТА НЕ ПОДХОДИТ: {why}");
            println!("Зритель в этом случае сам перейдёт на процессор. Проверяю запасной путь (чтение кадра с видеокарты + swscale)...");
            let owned = video::readback(&*hwf).context("запасной путь: чтение кадра")?;
            let mut fpx = Vec::new();
            cpu.convert(owned.get(), dw, dh, &mut fpx)?;
            let d = gpulogic::mean_abs_diff(&fpx, &cpx, dw as usize, dh as usize);
            println!("запасной путь работает, расхождение с эталоном {d:.1} из 255");
            av_frame_free(&mut sw);
            av_frame_free(&mut hwf);
            return Ok(());
        }
        let mut gpu = gpu.unwrap();
        let diff = gpulogic::mean_abs_diff(&gpx, &cpx, dw as usize, dh as usize);
        let bad = !diff.is_finite() || diff > gpulogic::MAX_MEAN_DIFF;
        let n = 30;
        let t = Instant::now();
        for _ in 0..n {
            gpu.convert(&*hwf, dw, dh, &mut gpx)?;
        }
        let g_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t = Instant::now();
        for _ in 0..n {
            cpu.convert(&*sw, dw, dh, &mut cpx)?;
        }
        let c_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t = Instant::now();
        let mut tmp = av_frame_alloc();
        for _ in 0..n {
            av_frame_unref(tmp);
            av_hwframe_transfer_data(tmp, hwf, 0);
        }
        let rb_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        av_frame_free(&mut tmp);
        println!("{vw}x{vh} → {dw}x{dh}");
        println!("видеокарта (video processor + чтение результата): {g_ms:.1} мс/кадр");
        println!("процессор (swscale, 1 поток): {c_ms:.1} мс/кадр; плюс чтение кадра с видеокарты: {rb_ms:.1} мс");
        println!("расхождение картинок: {diff:.1} из 255 (допустимо до {})", gpulogic::MAX_MEAN_DIFF);
        let mid = |p: &[u32]| p[(dh as usize / 4) * dw as usize + dw as usize * 3 / 16]; // the 2nd colour bar
        println!("пример пикселя (2-я полоса): видеокарта {:06x}, процессор {:06x}", mid(&gpx), mid(&cpx));
        av_frame_free(&mut sw);
        av_frame_free(&mut hwf);
        if bad {
            bail!("КАРТИНКИ НЕ СХОДЯТСЯ — зритель сам перейдёт на процессор");
        }
        println!("ВИДЕОКАРТА ПОДХОДИТ: цвет и масштаб на ней работают верно");
    }
    Ok(())
}
