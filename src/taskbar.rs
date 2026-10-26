// A red dot on the program's taskbar button while the screen is being shown (ITaskbarList3::SetOverlayIcon,
// the same badge Discord or Telegram use). It is set and cleared from the GUI thread; any failure just means no
// badge, never an error the user sees.
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO};

const SIZE: i32 = 16;

/// Premultiplied BGRA pixels of the dot: red, anti-aliased, with a thin light ring so it reads on dark and light
/// taskbars alike. Pure function, tested.
pub fn dot_pixels() -> Vec<u32> {
    let c = (SIZE as f32 - 1.0) / 2.0;
    let mut px = vec![0u32; (SIZE * SIZE) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let cover = (7.5 - d).clamp(0.0, 1.0); // whole disc, radius 7.5
            if cover <= 0.0 {
                continue;
            }
            let ring = (d - 5.6).clamp(0.0, 1.0); // outer ~2 px: light ring
            let (r, g, b) = (
                0xE5 as f32 * (1.0 - ring) + 0xFF as f32 * ring,
                0x2B as f32 * (1.0 - ring) + 0xFF as f32 * ring,
                0x2B as f32 * (1.0 - ring) + 0xFF as f32 * ring,
            );
            let a = cover;
            px[(y * SIZE + x) as usize] =
                ((a * 255.0) as u32) << 24 | ((r * a) as u32) << 16 | ((g * a) as u32) << 8 | (b * a) as u32;
        }
    }
    px
}

fn make_icon() -> Option<HICON> {
    unsafe {
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SIZE,
            biHeight: -SIZE, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let px = dot_pixels();
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
        let mask = CreateBitmap(SIZE, SIZE, 1, 1, None); // all zero: alpha decides
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

#[derive(Default)]
pub struct Badge {
    list: Option<ITaskbarList3>,
    icon: Option<HICON>,
    shown: Option<bool>,
    failed: bool,
}

impl Badge {
    /// Shows (`live`) or clears the dot on the taskbar button of `hwnd`. Cheap when nothing changes.
    pub fn set(&mut self, hwnd: isize, live: bool) {
        if self.failed || self.shown == Some(live) {
            return;
        }
        unsafe {
            if self.list.is_none() {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); // already initialised by the window: fine
                match CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) {
                    Ok(l) if l.HrInit().is_ok() => self.list = Some(l),
                    _ => {
                        self.failed = true;
                        return;
                    }
                }
            }
            if live && self.icon.is_none() {
                self.icon = make_icon();
            }
            let list = self.list.as_ref().unwrap();
            let h = HWND(hwnd as *mut core::ffi::c_void);
            let r = match (live, self.icon) {
                (true, Some(icon)) => list.SetOverlayIcon(h, icon, w!("Идёт показ экрана")),
                _ => list.SetOverlayIcon(h, HICON::default(), PCWSTR::null()),
            };
            if r.is_ok() {
                self.shown = Some(live);
            }
        }
    }
}

impl Drop for Badge {
    fn drop(&mut self) {
        if let Some(i) = self.icon.take() {
            unsafe {
                let _ = DestroyIcon(i);
            }
        }
    }
}

/// `badgetest`: a plain window with the red dot on its taskbar button for `secs` seconds (to look at it).
pub fn demo(secs: u64) -> anyhow::Result<()> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::time::{Duration, Instant};
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::window::{Window, WindowId};
    struct App {
        window: Option<Window>,
        badge: Badge,
        until: Instant,
    }
    impl ApplicationHandler for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            let w = el.create_window(Window::default_attributes().with_title("screenshare — значок трансляции")).expect("window");
            self.window = Some(w);
        }
        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
            if let WindowEvent::CloseRequested = ev {
                el.exit();
            }
        }
        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            if let Some(w) = &self.window {
                if let Ok(h) = w.window_handle() {
                    if let RawWindowHandle::Win32(h) = h.as_raw() {
                        self.badge.set(h.hwnd.get(), true);
                    }
                }
            }
            if Instant::now() > self.until {
                el.exit();
            }
            el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(200)));
        }
    }
    let el = EventLoop::new()?;
    el.run_app(&mut App { window: None, badge: Badge::default(), until: Instant::now() + Duration::from_secs(secs) })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_is_a_red_disc_with_a_light_rim_and_clear_corners() {
        let px = dot_pixels();
        let at = |x: i32, y: i32| px[(y * SIZE + x) as usize];
        assert_eq!(at(0, 0) >> 24, 0, "corners transparent");
        let centre = at(7, 7);
        assert_eq!(centre >> 24, 255);
        let (r, g) = ((centre >> 16) & 0xFF, (centre >> 8) & 0xFF);
        assert!(r > 200 && g < 80, "red centre: {centre:08x}");
        let rim = at(7, 1);
        assert!((rim >> 8) & 0xFF > 150, "light rim: {rim:08x}");
        // premultiplied: no channel exceeds alpha
        assert!(px.iter().all(|&p| [(p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF].iter().all(|&c| c <= p >> 24)));
    }
}
