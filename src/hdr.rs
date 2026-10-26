// HDR desktop -> SDR picture, Windows side (the math is in hdrmath.rs). See the comment there for the "why".
//
//  * `probe_monitor` asks Windows (DisplayConfig) whether "Use HDR" / advanced colour is on for a monitor and
//    what its SDR white level is (the "SDR content brightness" slider).
//  * `Watcher` re-asks every 2 s on a background thread, so the capture thread never waits for DisplayConfig
//    (on an SDR PC the capture path is exactly what it was; it only reads one atomic per frame).
//  * `ToneMapper` turns a captured scRGB (float16) or PQ (R10G10B10A2) texture into a BGRA8 texture on the GPU
//    with a small pixel shader (same curve as hdrmath.rs). Its output goes through the unchanged BGRA8 path.
//    If it can't be set up, the callers use hdrmath's CPU conversion instead.
use crate::hdrmath::{self, HdrMode};
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use windows::core::{Interface, PCSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// Hidden switch: env SCREENSHARE_HDR = auto (default) | off | <nits> (force the SDR white level).
pub fn mode() -> HdrMode {
    static M: OnceLock<HdrMode> = OnceLock::new();
    *M.get_or_init(|| hdrmath::parse_mode(std::env::var("SCREENSHARE_HDR").ok().as_deref()))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrInfo {
    /// Windows composes this monitor in HDR / advanced colour (the capture is then scRGB, not sRGB).
    pub active: bool,
    pub white_nits: f32,
}

impl HdrInfo {
    pub const SDR: HdrInfo = HdrInfo { active: false, white_nits: hdrmath::DEFAULT_SDR_WHITE_NITS };
    pub fn gain(&self) -> f32 {
        hdrmath::gain_for_white(self.white_nits)
    }
    fn pack(&self) -> u32 {
        // bit 31 = active, rest = white level in 1/16 nit
        ((self.active as u32) << 31) | ((self.white_nits.clamp(0.0, 4000.0) * 16.0) as u32 & 0x7FFF_FFFF)
    }
    fn unpack(v: u32) -> HdrInfo {
        HdrInfo { active: v >> 31 != 0, white_nits: (v & 0x7FFF_FFFF) as f32 / 16.0 }
    }
}

pub fn is_hdr_format(f: DXGI_FORMAT) -> bool {
    f == DXGI_FORMAT_R16G16B16A16_FLOAT || f == DXGI_FORMAT_R10G10B10A2_UNORM
}

/// HDR state of one monitor. Any failure means "not HDR" (the SDR path then runs as it always did).
pub fn probe_monitor(hmon: HMONITOR) -> HdrInfo {
    if mode() == HdrMode::Off || hmon.is_invalid() {
        return HdrInfo::SDR;
    }
    let mut info = unsafe { probe_inner(hmon) }.unwrap_or(HdrInfo::SDR);
    if let HdrMode::White(n) = mode() {
        info.white_nits = n;
    }
    info
}

unsafe fn probe_inner(hmon: HMONITOR) -> Option<HdrInfo> {
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if !GetMonitorInfoW(hmon, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
        return None;
    }
    let (mut np, mut nm) = (0u32, 0u32);
    if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).0 != 0 || np == 0 {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).0 != 0 {
        return None;
    }
    let want = &mi.szDevice[..mi.szDevice.iter().position(|&c| c == 0).unwrap_or(32)];
    for p in &paths[..np as usize] {
        let mut sn = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        sn.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        sn.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        sn.header.adapterId = p.sourceInfo.adapterId;
        sn.header.id = p.sourceInfo.id;
        if DisplayConfigGetDeviceInfo(&mut sn.header) != 0 {
            continue;
        }
        let got = &sn.viewGdiDeviceName[..sn.viewGdiDeviceName.iter().position(|&c| c == 0).unwrap_or(32)];
        if got != want {
            continue;
        }
        let mut ac = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
        ac.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
        ac.header.size = std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
        ac.header.adapterId = p.targetInfo.adapterId;
        ac.header.id = p.targetInfo.id;
        if DisplayConfigGetDeviceInfo(&mut ac.header) != 0 {
            return None;
        }
        let enabled = ac.Anonymous.value & 0b10 != 0; // bit 1: advancedColorEnabled
        if !enabled {
            return Some(HdrInfo::SDR);
        }
        let mut wl = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
        wl.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
        wl.header.size = std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
        wl.header.adapterId = p.targetInfo.adapterId;
        wl.header.id = p.targetInfo.id;
        let nits = if DisplayConfigGetDeviceInfo(&mut wl.header) == 0 && wl.SDRWhiteLevel > 0 {
            wl.SDRWhiteLevel as f32 / 1000.0 * 80.0 // 1000 = 80 nits
        } else {
            hdrmath::DEFAULT_SDR_WHITE_NITS
        };
        return Some(HdrInfo { active: true, white_nits: nits });
    }
    None
}

/// Which monitor a capture is on.
#[derive(Clone, Copy)]
pub enum Target {
    /// The monitor a window is mostly on (follows the window when it is moved).
    Window(isize),
    /// The monitor that contains this desktop point.
    Point(i32, i32),
}

fn monitor_of(t: Target) -> HMONITOR {
    unsafe {
        match t {
            Target::Window(h) => MonitorFromWindow(HWND(h as *mut c_void), MONITOR_DEFAULTTONEAREST),
            Target::Point(x, y) => MonitorFromPoint(windows::Win32::Foundation::POINT { x, y }, MONITOR_DEFAULTTONEAREST),
        }
    }
}

/// Keeps the HDR state of a capture target up to date (refreshed every 2 s on its own thread).
pub struct Watcher {
    state: Arc<AtomicU32>,
}

impl Watcher {
    pub fn start(t: Target) -> Watcher {
        let first = if mode() == HdrMode::Off { HdrInfo::SDR } else { probe_monitor(monitor_of(t)) };
        let state = Arc::new(AtomicU32::new(first.pack()));
        if mode() != HdrMode::Off {
            let weak = Arc::downgrade(&state);
            std::thread::Builder::new()
                .name("hdr-watch".into())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let Some(s) = weak.upgrade() else { break };
                    s.store(probe_monitor(monitor_of(t)).pack(), Ordering::Relaxed);
                })
                .ok();
        }
        Watcher { state }
    }
    pub fn get(&self) -> HdrInfo {
        HdrInfo::unpack(self.state.load(Ordering::Relaxed))
    }
}

const HLSL: &str = r#"
cbuffer P : register(b0) { float gain; uint mode; float2 pad; };
Texture2D<float4> src : register(t0);
struct VSO { float4 pos : SV_Position; };
VSO vs(uint id : SV_VertexID) {
    float2 uv = float2((id << 1) & 2, id & 2);
    VSO o;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
static const float KNEE = __KNEE__;
float fixnan(float x) { return (x == x) ? x : 0.0; }
float srgb(float l) {
    l = saturate(l);
    return (l <= 0.0031308) ? 12.92 * l : 1.055 * pow(l, 1.0 / 2.4) - 0.055;
}
float pq(float e) {
    const float M1 = 2610.0 / 16384.0;
    const float M2 = 2523.0 / 4096.0 * 128.0;
    const float C1 = 3424.0 / 4096.0;
    const float C2 = 2413.0 / 4096.0 * 32.0;
    const float C3 = 2392.0 / 4096.0 * 32.0;
    e = saturate(fixnan(e));
    float p = pow(e, 1.0 / M2);
    float n = max(p - C1, 0.0);
    float d = C2 - C3 * p;
    return pow(n / d, 1.0 / M1) * 125.0;
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float3 v = src.Load(int3(int2(pos.xy), 0)).rgb;
    if (mode == 1) {
        v = float3(pq(v.r), pq(v.g), pq(v.b));
        v = mul(float3x3(1.660491, -0.587641, -0.072850,
                        -0.124550,  1.132900, -0.008349,
                        -0.018151, -0.100579,  1.118730), v);
    }
    v = float3(fixnan(v.r), fixnan(v.g), fixnan(v.b));
    v = clamp(v * gain, 0.0, 10000.0);
    float m = max(v.r, max(v.g, v.b));
    float s = 1.0;
    if (m > KNEE) {
        float t = (m - KNEE) / (1.0 - KNEE);
        s = (1.0 - (1.0 - KNEE) / (1.0 + t)) / m;
    }
    v *= s;
    return float4(srgb(v.r), srgb(v.g), srgb(v.b), 1.0);
}
"#;

type D3DCompileFn = unsafe extern "system" fn(
    *const c_void,
    usize,
    PCSTR,
    *const c_void,
    *const c_void,
    PCSTR,
    PCSTR,
    u32,
    u32,
    *mut *mut c_void,
    *mut *mut c_void,
) -> windows::core::HRESULT;

/// Compiles HLSL with the system's d3dcompiler_47.dll (present on Windows 10/11), looked up at run time so the
/// exe has no extra import. Returns the bytecode.
unsafe fn compile(src: &str, entry: &str, target: &str) -> Result<Vec<u8>> {
    let lib = LoadLibraryW(windows::core::w!("d3dcompiler_47.dll")).context("нет d3dcompiler_47.dll")?;
    let f = GetProcAddress(lib, windows::core::s!("D3DCompile")).ok_or_else(|| anyhow!("нет D3DCompile"))?;
    let f: D3DCompileFn = std::mem::transmute(f);
    let (e, t) = (format!("{entry}\0"), format!("{target}\0"));
    let (mut code, mut err): (*mut c_void, *mut c_void) = (std::ptr::null_mut(), std::ptr::null_mut());
    // 1<<13 = D3DCOMPILE_IEEE_STRICTNESS: keep the NaN checks
    let hr = f(
        src.as_ptr() as *const c_void,
        src.len(),
        PCSTR::null(),
        std::ptr::null(),
        std::ptr::null(),
        PCSTR(e.as_ptr()),
        PCSTR(t.as_ptr()),
        1 << 13,
        0,
        &mut code,
        &mut err,
    );
    let err_blob = (!err.is_null()).then(|| ID3DBlob::from_raw(err));
    if hr.is_err() || code.is_null() {
        let msg = err_blob
            .map(|b| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).trim().to_string())
            .unwrap_or_default();
        bail!("шейдер не скомпилировался ({entry}): {msg} {hr:?}");
    }
    let blob = ID3DBlob::from_raw(code);
    Ok(std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec())
}

struct SrcTex {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    key: (u32, u32, i32),
}
struct OutTex {
    tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    key: (u32, u32),
}

/// scRGB float16 / PQ 10-bit texture -> BGRA8 texture, on the GPU of `dev`. Used from one thread only (the
/// capture thread that owns `dev`).
pub struct ToneMapper {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    src: Option<SrcTex>,
    out: Option<OutTex>,
}

impl ToneMapper {
    pub fn new(dev: &ID3D11Device, ctx: &ID3D11DeviceContext) -> Result<ToneMapper> {
        unsafe {
            let hlsl = HLSL.replace("__KNEE__", &format!("{:?}", hdrmath::KNEE));
            let vsb = compile(&hlsl, "vs", "vs_4_0")?;
            let psb = compile(&hlsl, "ps", "ps_4_0")?;
            let mut vs = None;
            dev.CreateVertexShader(&vsb, None, Some(&mut vs)).context("CreateVertexShader")?;
            let mut ps = None;
            dev.CreatePixelShader(&psb, None, Some(&mut ps)).context("CreatePixelShader")?;
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let mut cb = None;
            dev.CreateBuffer(&bd, None, Some(&mut cb)).context("CreateBuffer")?;
            Ok(ToneMapper {
                dev: dev.clone(),
                ctx: ctx.clone(),
                vs: vs.ok_or_else(|| anyhow!("CreateVertexShader: пусто"))?,
                ps: ps.ok_or_else(|| anyhow!("CreatePixelShader: пусто"))?,
                cb: cb.ok_or_else(|| anyhow!("CreateBuffer: пусто"))?,
                src: None,
                out: None,
            })
        }
    }

    /// Renders `input` (R16G16B16A16_FLOAT or R10G10B10A2_UNORM) into an own BGRA8 texture of the same size and
    /// returns it (valid until the next call). `gain` is `HdrInfo::gain()`.
    pub fn run(&mut self, input: &ID3D11Texture2D, gain: f32) -> Result<ID3D11Texture2D> {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            input.GetDesc(&mut d);
            let (w, h) = (d.Width, d.Height);
            if !is_hdr_format(d.Format) || w == 0 || h == 0 {
                bail!("формат кадра {} не HDR", d.Format.0);
            }
            if self.src.as_ref().is_none_or(|s| s.key != (w, h, d.Format.0)) {
                self.src = None;
                let sd = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: d.Format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.dev.CreateTexture2D(&sd, None, Some(&mut t)).context("CreateTexture2D(HDR копия)")?;
                let tex = t.ok_or_else(|| anyhow!("CreateTexture2D: пусто"))?;
                let mut srv = None;
                self.dev.CreateShaderResourceView(&tex, None, Some(&mut srv)).context("CreateShaderResourceView")?;
                self.src = Some(SrcTex { tex, srv: srv.ok_or_else(|| anyhow!("SRV: пусто"))?, key: (w, h, d.Format.0) });
            }
            if self.out.as_ref().is_none_or(|o| o.key != (w, h)) {
                self.out = None;
                let od = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                self.dev.CreateTexture2D(&od, None, Some(&mut t)).context("CreateTexture2D(SDR выход)")?;
                let tex = t.ok_or_else(|| anyhow!("CreateTexture2D: пусто"))?;
                let mut rtv = None;
                self.dev.CreateRenderTargetView(&tex, None, Some(&mut rtv)).context("CreateRenderTargetView")?;
                self.out = Some(OutTex { tex, rtv: rtv.ok_or_else(|| anyhow!("RTV: пусто"))?, key: (w, h) });
            }
            let (src, out) = (self.src.as_ref().unwrap(), self.out.as_ref().unwrap());
            self.ctx.CopyResource(&src.tex, input);
            let mode = if d.Format == DXGI_FORMAT_R10G10B10A2_UNORM { 1u32 } else { 0 };
            let mut cbv = [0u8; 16];
            cbv[..4].copy_from_slice(&gain.to_le_bytes());
            cbv[4..8].copy_from_slice(&mode.to_le_bytes());
            self.ctx.UpdateSubresource(&self.cb, 0, None, cbv.as_ptr() as *const c_void, 0, 0);
            let c = &self.ctx;
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            c.PSSetShaderResources(0, Some(&[Some(src.srv.clone())]));
            c.OMSetRenderTargets(Some(&[Some(out.rtv.clone())]), None);
            c.OMSetBlendState(None, None, 0xFFFF_FFFF);
            c.RSSetState(None);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            c.Draw(3, 0);
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(Some(&[None]), None);
            Ok(out.tex.clone())
        }
    }
}

/// `screenshare hdrtest`: checks the HDR path without any capture. Prints what Windows says about every monitor,
/// then renders a synthetic scRGB float16 picture through the GPU tone mapper and through the CPU fallback and
/// compares the two.
pub fn selftest() -> Result<()> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    for (i, m) in crate::capture::list_monitors()?.iter().enumerate() {
        let hmon = monitor_of(Target::Point(m.left + 1, m.top + 1));
        let i2 = probe_monitor(hmon);
        println!(
            "монитор {i}: {}x{} — HDR {} (белый SDR {:.0} нит, множитель {:.3})",
            m.width,
            m.height,
            if i2.active { "ВКЛЮЧЁН" } else { "выключен" },
            i2.white_nits,
            i2.gain()
        );
    }
    // synthetic picture: grey ramp 0..4 in scRGB (SDR white of a 240-nit desktop is 3.0) + colour patches
    let (w, h) = (256usize, 64usize);
    let to_h = |v: f32| -> u16 {
        // f32 -> half (round to nearest, values are normal and small here)
        let b = v.to_bits();
        let s = ((b >> 16) & 0x8000) as u16;
        let e = ((b >> 23) & 0xFF) as i32 - 127 + 15;
        let m = (b >> 13) & 0x3FF;
        if v == 0.0 { s } else if e <= 0 { s } else if e >= 31 { s | 0x7BFF } else { s | ((e as u16) << 10) | m as u16 }
    };
    let mut data = vec![0u8; w * h * 8];
    for y in 0..h {
        for x in 0..w {
            let g = x as f32 / (w - 1) as f32 * 4.0;
            let (r, gg, b) = if y < h / 2 { (g, g, g) } else { (g, g * 0.5, g * 0.1) };
            for (c, v) in [r, gg, b, 1.0].into_iter().enumerate() {
                data[(y * w + x) * 8 + c * 2..][..2].copy_from_slice(&to_h(v).to_le_bytes());
            }
        }
    }
    let gain = hdrmath::gain_for_white(240.0);
    let mut cpu = vec![0u8; w * h * 4];
    hdrmath::convert_f16_to_bgra(&data, w * 8, w, h, gain, &mut cpu);

    unsafe {
        let (mut dev, mut ctx) = (None, None);
        D3D11CreateDevice(
            None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )
        .context("D3D11CreateDevice")?;
        let (dev, ctx) = (dev.unwrap(), ctx.unwrap());
        let mut tm = match ToneMapper::new(&dev, &ctx) {
            Ok(t) => t,
            Err(e) => {
                println!("GPU-путь недоступен ({e:#}); будет работать запасной путь на процессоре — он проверен только расчётом");
                return Ok(());
            }
        };
        let td = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, SysMemPitch: (w * 8) as u32, SysMemSlicePitch: 0 };
        let mut input = None;
        dev.CreateTexture2D(&td, Some(&init), Some(&mut input)).context("CreateTexture2D(вход)")?;
        let input = input.ok_or_else(|| anyhow!("вход: пусто"))?;
        let out = tm.run(&input, gain)?;
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        out.GetDesc(&mut sd);
        sd.Usage = D3D11_USAGE_STAGING;
        sd.BindFlags = 0;
        sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut st = None;
        dev.CreateTexture2D(&sd, None, Some(&mut st))?;
        let st = st.ok_or_else(|| anyhow!("staging: пусто"))?;
        ctx.CopyResource(&st, &out);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let (mut maxd, mut sum) = (0i32, 0u64);
        for y in 0..h {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), w * 4);
            for i in 0..w * 4 {
                if i % 4 == 3 {
                    continue;
                }
                let d = (row[i] as i32 - cpu[y * w * 4 + i] as i32).abs();
                maxd = maxd.max(d);
                sum += d as u64;
            }
        }
        let px = |x: usize| {
            let row = std::slice::from_raw_parts(m.pData as *const u8, w * 4);
            [row[x * 4 + 2], row[x * 4 + 1], row[x * 4]]
        };
        println!(
            "серый 0.75 → {:?}, 3.0 (белый SDR при 240 нит) → {:?}, 4.0 → {:?} (R,G,B)",
            px(w * 3 / 16),
            px(w * 3 / 4),
            px(w - 1)
        );
        ctx.Unmap(&st, 0);
        println!("GPU против процессора: максимум {maxd}, среднее {:.2} из 255", sum as f64 / (w * h * 3) as f64);
        if maxd > 2 {
            bail!("GPU и процессор расходятся (макс. {maxd}) — HDR-картинка может быть неверной");
        }
        println!("HDR-ПУТЬ РАБОТАЕТ: расчёты видеокарты и процессора совпадают");
    }
    Ok(())
}
