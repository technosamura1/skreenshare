// Windows 11 glass for the launcher window (Mica backdrop) and the dark title bar. Everything here is optional:
// on an older Windows, or when anything fails, the window just stays flat. The viewer window is never touched
// (it is the video; nothing may sit between it and the screen).

use std::ffi::c_void;

#[repr(C)]
struct OsVersionInfo {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    csd: [u16; 128],
}

/// Windows build number (RtlGetVersion: the honest one, GetVersionEx lies without a manifest).
pub fn build_number() -> Option<u32> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfo) -> i32;
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let f = GetProcAddress(ntdll, s!("RtlGetVersion"))?;
        let f: RtlGetVersion = std::mem::transmute(f);
        let mut v = OsVersionInfo { size: std::mem::size_of::<OsVersionInfo>() as u32, major: 0, minor: 0, build: 0, platform: 0, csd: [0; 128] };
        (f(&mut v) == 0).then_some(v.build)
    }
}

/// The system backdrop attribute exists from Windows 11 22H2 (build 22621). `SCREENSHARE_GLASS_FORCE` (any value)
/// skips the check, for trying the code path on something that is not Windows 11 (wine).
pub fn supported() -> bool {
    std::env::var_os("SCREENSHARE_GLASS_FORCE").is_some() || build_number().is_some_and(|b| b >= 22621)
}

/// Dark or light title bar and, when `mica`, the Mica backdrop behind the whole client area.
pub fn apply(hwnd: isize, dark: bool, mica: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
    use windows::Win32::UI::Controls::MARGINS;
    if hwnd == 0 {
        return;
    }
    let hwnd = HWND(hwnd as *mut c_void);
    let set = |attr: i32, value: i32| unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(attr), &value as *const i32 as *const c_void, 4);
    };
    set(20, dark as i32); // DWMWA_USE_IMMERSIVE_DARK_MODE
    if mica {
        set(38, 2); // DWMWA_SYSTEMBACKDROP_TYPE = DWMSBT_MAINWINDOW (Mica)
        let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
        unsafe {
            let _ = DwmExtendFrameIntoClientArea(hwnd, &m);
        }
    }
}
