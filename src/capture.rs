// Screen capture via DXGI Desktop Duplication. Frames are copied to CPU memory (BGRA), which also
// works when the monitor hangs off a different GPU (e.g. iGPU) than the one doing NVENC.
// Desktop Duplication does not include the mouse cursor, so we draw it ourselves.
use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct MonitorInfo {
    pub adapter: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    adapter_idx: u32,
    output_idx: u32,
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let desc = adapter.GetDesc1()?;
            let name = wide_to_string(&desc.Description);
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                let od = output.GetDesc()?;
                if od.AttachedToDesktop.as_bool() {
                    let r = od.DesktopCoordinates;
                    out.push(MonitorInfo {
                        adapter: name.clone(),
                        width: (r.right - r.left) as u32,
                        height: (r.bottom - r.top) as u32,
                        primary: r.left == 0 && r.top == 0,
                        adapter_idx: ai,
                        output_idx: oi,
                    });
                }
                oi += 1;
            }
            ai += 1;
        }
    }
    Ok(out)
}

struct Cursor {
    visible: bool,
    pos: POINT,
    shape: Vec<u8>,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
}

pub struct Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dupl: Option<IDXGIOutputDuplication>,
    staging: Option<ID3D11Texture2D>,
    /// Desktop orientation (what the user sees); for rotated monitors the texture is tex_w x tex_h.
    pub width: u32,
    pub height: u32,
    tex_w: u32,
    tex_h: u32,
    rotation: DXGI_MODE_ROTATION,
    /// Latest desktop image, tightly packed BGRA.
    desktop: Vec<u8>,
    cursor: Cursor,
}

pub enum Event {
    /// Nothing changed within the timeout.
    Timeout,
    /// The image and/or the cursor changed.
    Updated,
    /// Duplication was lost (UAC prompt, mode change, ...) and recreated; size may differ.
    Reset,
    /// Window capture only: the captured window no longer exists.
    Closed,
}

impl Capturer {
    pub fn new(monitor: &MonitorInfo) -> Result<Capturer> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = factory.EnumAdapters1(monitor.adapter_idx)?;
            let output: IDXGIOutput1 = adapter.EnumOutputs(monitor.output_idx)?.cast()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let mut c = Capturer {
                device: device.unwrap(),
                context: context.unwrap(),
                output,
                dupl: None,
                staging: None,
                width: 0,
                height: 0,
                tex_w: 0,
                tex_h: 0,
                rotation: DXGI_MODE_ROTATION_IDENTITY,
                desktop: Vec::new(),
                cursor: Cursor { visible: false, pos: POINT::default(), shape: Vec::new(), info: Default::default() },
            };
            c.reinit()?;
            Ok(c)
        }
    }

    fn reinit(&mut self) -> Result<()> {
        self.dupl = None;
        self.staging = None;
        unsafe {
            let dupl = self
                .output
                .DuplicateOutput(&self.device)
                .context("DuplicateOutput (захват экрана недоступен)")?;
            let desc = dupl.GetDesc();
            // ModeDesc is already in desktop orientation; the acquired texture is in scan-out orientation.
            self.width = desc.ModeDesc.Width;
            self.height = desc.ModeDesc.Height;
            self.rotation = desc.Rotation;
            let swapped = self.rotation == DXGI_MODE_ROTATION_ROTATE90 || self.rotation == DXGI_MODE_ROTATION_ROTATE270;
            (self.tex_w, self.tex_h) = if swapped { (self.height, self.width) } else { (self.width, self.height) };
            self.dupl = Some(dupl);
        }
        self.desktop = vec![0; (self.width * self.height * 4) as usize];
        Ok(())
    }

    pub fn next(&mut self, timeout_ms: u32) -> Result<Event> {
        if self.dupl.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(100));
            return Ok(match self.reinit() {
                Ok(()) => Event::Reset,
                Err(_) => Event::Timeout,
            });
        }
        let dupl = self.dupl.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dupl.AcquireNextFrame(timeout_ms, &mut info, &mut res) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Event::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                self.dupl = None;
                return Ok(match self.reinit() {
                    Ok(()) => Event::Reset,
                    Err(_) => Event::Timeout,
                });
            }
            Err(e) => bail!("AcquireNextFrame: {e}"),
        }
        let r = self.process(&dupl, &info, res);
        unsafe {
            let _ = dupl.ReleaseFrame();
        }
        r?;
        Ok(Event::Updated)
    }

    fn process(
        &mut self,
        dupl: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        res: Option<IDXGIResource>,
    ) -> Result<()> {
        unsafe {
            if info.LastPresentTime != 0 {
                if let Some(res) = res {
                    let tex: ID3D11Texture2D = res.cast()?;
                    if self.staging.is_none() {
                        let mut d = D3D11_TEXTURE2D_DESC::default();
                        tex.GetDesc(&mut d);
                        if (d.Width, d.Height) != (self.tex_w, self.tex_h) {
                            bail!("неожиданный размер кадра {}x{} (ждал {}x{})", d.Width, d.Height, self.tex_w, self.tex_h);
                        }
                        d.Usage = D3D11_USAGE_STAGING;
                        d.BindFlags = 0;
                        d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                        d.MiscFlags = 0;
                        d.MipLevels = 1;
                        d.ArraySize = 1;
                        let mut st = None;
                        self.device.CreateTexture2D(&d, None, Some(&mut st))?;
                        self.staging = st;
                    }
                    let st = self.staging.clone().unwrap();
                    self.context.CopyResource(&st, &tex);
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    self.context.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
                    let src = std::slice::from_raw_parts(m.pData as *const u8, m.RowPitch as usize * self.tex_h as usize);
                    self.copy_rotated(src, m.RowPitch as usize);
                    self.context.Unmap(&st, 0);
                }
            }
            if info.LastMouseUpdateTime != 0 {
                self.cursor.visible = info.PointerPosition.Visible.as_bool();
                self.cursor.pos = info.PointerPosition.Position;
            }
            if info.PointerShapeBufferSize > 0 {
                let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
                let mut needed = 0u32;
                let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                if dupl
                    .GetFramePointerShape(buf.len() as u32, buf.as_mut_ptr() as *mut _, &mut needed, &mut si)
                    .is_ok()
                {
                    self.cursor.shape = buf;
                    self.cursor.info = si;
                }
            }
        }
        Ok(())
    }

    /// Texture (scan-out orientation) -> desktop buffer (what the user sees).
    fn copy_rotated(&mut self, src: &[u8], pitch: usize) {
        let (w, h) = (self.width as usize, self.height as usize);
        let (tw, th) = (self.tex_w as usize, self.tex_h as usize);
        let px = |tx: usize, ty: usize| -> [u8; 4] { src[ty * pitch + tx * 4..][..4].try_into().unwrap() };
        let r = self.rotation;
        if r == DXGI_MODE_ROTATION_ROTATE90 {
            for dy in 0..h {
                for dx in 0..w {
                    let p = px(dy, th - 1 - dx);
                    self.desktop[(dy * w + dx) * 4..][..4].copy_from_slice(&p);
                }
            }
        } else if r == DXGI_MODE_ROTATION_ROTATE270 {
            for dy in 0..h {
                for dx in 0..w {
                    let p = px(tw - 1 - dy, dx);
                    self.desktop[(dy * w + dx) * 4..][..4].copy_from_slice(&p);
                }
            }
        } else if r == DXGI_MODE_ROTATION_ROTATE180 {
            for dy in 0..h {
                for dx in 0..w {
                    let p = px(tw - 1 - dx, th - 1 - dy);
                    self.desktop[(dy * w + dx) * 4..][..4].copy_from_slice(&p);
                }
            }
        } else {
            let row = w * 4;
            for y in 0..h {
                self.desktop[y * row..(y + 1) * row].copy_from_slice(&src[y * pitch..y * pitch + row]);
            }
        }
    }

    /// Writes the current desktop plus cursor into `dst` (BGRA rows of `stride` bytes).
    pub fn compose(&self, dst: &mut [u8], stride: usize) {
        let row = (self.width * 4) as usize;
        for y in 0..self.height as usize {
            dst[y * stride..y * stride + row].copy_from_slice(&self.desktop[y * row..(y + 1) * row]);
        }
        if self.cursor.visible && !self.cursor.shape.is_empty() {
            draw_cursor(dst, stride, self.width as i32, self.height as i32, &self.cursor);
        }
    }
}

fn draw_cursor(dst: &mut [u8], stride: usize, w: i32, h: i32, c: &Cursor) {
    let si = &c.info;
    let pitch = si.Pitch as usize;
    let cw = si.Width as i32;
    let ty = si.Type as i32;
    let is_mono = ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0;
    let ch = if is_mono { si.Height as i32 / 2 } else { si.Height as i32 };
    for yy in 0..ch {
        let py = c.pos.y + yy;
        if py < 0 || py >= h {
            continue;
        }
        for xx in 0..cw {
            let px = c.pos.x + xx;
            if px < 0 || px >= w {
                continue;
            }
            let d = py as usize * stride + px as usize * 4;
            let p = &mut dst[d..d + 4];
            if is_mono {
                let bit = 0x80u8 >> (xx % 8);
                let byte = (xx / 8) as usize;
                let and = c.shape[yy as usize * pitch + byte] & bit != 0;
                let xor = c.shape[(yy + ch) as usize * pitch + byte] & bit != 0;
                for k in 0..3 {
                    let mut v = if and { p[k] } else { 0 };
                    if xor {
                        v ^= 0xFF;
                    }
                    p[k] = v;
                }
            } else {
                let s = &c.shape[yy as usize * pitch + xx as usize * 4..][..4];
                if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                    let a = s[3] as u32;
                    for k in 0..3 {
                        p[k] = ((s[k] as u32 * a + p[k] as u32 * (255 - a)) / 255) as u8;
                    }
                } else {
                    // MASKED_COLOR: alpha 0 = replace, 0xFF = XOR with screen.
                    for k in 0..3 {
                        p[k] = if s[3] == 0 { s[k] } else { p[k] ^ s[k] };
                    }
                }
            }
        }
    }
}
