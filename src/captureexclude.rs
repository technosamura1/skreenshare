//! Separate exclusion checks for WGC and DXGI Desktop Duplication.
use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, GetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};

#[derive(Clone, Copy, Debug, Default)]
pub struct Exclusion { pub wgc: bool, pub dda: bool }

#[repr(C)]
struct CompositionData { attribute: i32, data: *mut core::ffi::c_void, size: usize }
type CompositionFn = unsafe extern "system" fn(HWND, *mut CompositionData) -> BOOL;

pub fn apply(hwnd: HWND) -> Exclusion {
    unsafe {
        let mut affinity = 0;
        let wgc = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok()
            && GetWindowDisplayAffinity(hwnd, &mut affinity).is_ok() && affinity == WDA_EXCLUDEFROMCAPTURE.0;
        // WDA alone is insufficient for DDA on some Windows/driver combinations.
        // WCA_EXCLUDED_FROM_DDA is documented for Windows 10 1709+.
        let dda = set_dda(hwnd);
        crate::net::conn_log(&format!("оверлей: исключение WGC={wgc}, Desktop Duplication={dda}, affinity={affinity:#x}"));
        Exclusion { wgc, dda }
    }
}

unsafe fn set_dda(hwnd: HWND) -> bool {
    unsafe {
        if crate::glass::build_number().is_none_or(|b| b < 16299) { return false; }
        let Ok(lib) = GetModuleHandleW(w!("user32.dll")) else { return false };
        let Some(set) = GetProcAddress(lib, s!("SetWindowCompositionAttribute")) else { return false };
        let Some(get) = GetProcAddress(lib, s!("GetWindowCompositionAttribute")) else { return false };
        let set: CompositionFn = core::mem::transmute(set);
        let get: CompositionFn = core::mem::transmute(get);
        let mut value = BOOL(1);
        let mut data = CompositionData { attribute: 24, data: (&mut value as *mut BOOL).cast(), size: core::mem::size_of::<BOOL>() };
        if !set(hwnd, &mut data).as_bool() { return false; }
        value = BOOL(0);
        get(hwnd, &mut data).as_bool() && value.as_bool()
    }
}

pub fn allows(exclusion: Option<Exclusion>, dda: bool) -> bool {
    exclusion.is_some_and(|e| if dda { e.dda } else { e.wgc })
}

pub fn use_wgc_monitor(build: Option<u32>, annotations: bool, forced: &str) -> bool {
    annotations && forced != "dd" && (forced == "wgc" || build.is_none_or(|b| b >= 20348))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_10_monitor_capture_avoids_wgc_border() {
        assert!(!use_wgc_monitor(Some(19045), true, ""));
        assert!(use_wgc_monitor(Some(22000), true, ""));
        assert!(!use_wgc_monitor(Some(22000), true, "dd"));
        assert!(use_wgc_monitor(Some(19045), true, "wgc"));
        assert!(!use_wgc_monitor(Some(22000), false, ""));
    }
    #[test]
    fn each_backend_requires_its_own_exclusion() {
        assert!(!allows(None, true)); assert!(!allows(None, false));
        let only_wgc = Some(Exclusion { wgc: true, dda: false });
        assert!(allows(only_wgc, false)); assert!(!allows(only_wgc, true));
        let only_dda = Some(Exclusion { wgc: false, dda: true });
        assert!(!allows(only_dda, false)); assert!(allows(only_dda, true));
    }
}

#[cfg(test)]
mod desktop_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::Win32::Graphics::Dwm::DwmFlush;
    use windows::Win32::Graphics::Gdi::{UpdateWindow, CreateSolidBrush};
    use windows::Win32::Foundation::COLORREF;
    unsafe extern "system" fn test_proc(h: HWND, m: u32, w: windows::Win32::Foundation::WPARAM, l: windows::Win32::Foundation::LPARAM) -> windows::Win32::Foundation::LRESULT {
        unsafe { DefWindowProcW(h, m, w, l) }
    }
    struct TestWindow(HWND);
    impl Drop for TestWindow { fn drop(&mut self) { unsafe { let _ = DestroyWindow(self.0); } } }
    #[test]
    #[ignore] // Briefly creates two small synthetic windows; no network or audio.
    fn dda_excludes_test_overlay() -> anyhow::Result<()> {
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
        let mons = crate::capture::list_monitors()?;
        let mon = mons.iter().find(|m| m.primary).unwrap_or(&mons[0]);
        let (x, y) = (mon.left + 100, mon.top + 100);
        unsafe {
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(test_proc), lpszClassName: w!("SSExcludeWhiteTest"), hbrBackground: CreateSolidBrush(COLORREF(0xffffff)), ..Default::default() });
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(test_proc), lpszClassName: w!("SSExcludeBlackTest"), hbrBackground: CreateSolidBrush(COLORREF(0)), ..Default::default() });
        }
        let base = TestWindow(unsafe { CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("SSExcludeWhiteTest"), w!("screenshare exclusion test background"), WS_POPUP | WS_VISIBLE,
            x, y, 320, 240, None, None, None, None)? });
        let front = TestWindow(unsafe { CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            w!("SSExcludeBlackTest"), w!("screenshare exclusion test overlay"), WS_POPUP | WS_VISIBLE,
            x + 40, y + 40, 160, 100, None, None, None, None)? });
        unsafe { SetLayeredWindowAttributes(front.0, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA)?;
            let _ = UpdateWindow(base.0); let _ = UpdateWindow(front.0);
            SetWindowPos(front.0, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)?;
        }
        let mut cap = crate::capture::Capturer::new(mon)?;
        let sample = |cap: &mut crate::capture::Capturer| -> anyhow::Result<[u8; 3]> {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(600) {
                unsafe { let _ = DwmFlush(); }
                cap.next(30)?;
            }
            let stride = cap.width as usize * 4;
            let mut pixels = vec![0; stride * cap.height as usize]; cap.compose(&mut pixels, stride);
            let pos = 160 * stride + 180 * 4;
            Ok([pixels[pos], pixels[pos + 1], pixels[pos + 2]])
        };
        let before = sample(&mut cap)?;
        assert!(before.iter().all(|c| *c < 30), "test overlay must be visible before exclusion: {before:?}");
        let excluded = apply(front.0);
        assert!(excluded.dda, "DDA exclusion not confirmed");
        let after = sample(&mut cap)?;
        assert!(after.iter().all(|c| *c > 220), "captured image must show white background, not overlay/black hole: {after:?}");
        println!("PASS: DDA overlay exclusion, before={before:?}, after={after:?}, flags={excluded:?}");
        Ok(())
    }
}
