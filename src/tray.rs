// The launcher in the notification area ("tray"). The close button hides the main window instead of quitting, so
// that only the viewer's button is left on the taskbar; the icon by the clock brings the window back (a click) or
// quits for real (its menu). The icon lives on its own thread with a hidden window of its own, because the main
// window's message loop belongs to winit. Everything here is optional: any failure just means no icon, and then
// the close button closes as before.
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, FindWindowW, GetCursorPos,
    GetMessageW, IsIconic, IsWindowVisible, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SendMessageW, SetForegroundWindow, ShowWindow, TrackPopupMenu, TranslateMessage, HICON, ICON_BIG, ICON_SMALL, IDI_APPLICATION,
    MF_STRING, MSG, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNOACTIVATE, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_GETICON, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WNDCLASSW,
};

const CLASS: PCWSTR = w!("screenshare-tray");
/// The icon's own callback message, and "another copy was started: show yourself".
const WM_TRAY: u32 = WM_APP + 1;
const WM_SHOW_ME: u32 = WM_APP + 2;
const CMD_OPEN: usize = 1;
const CMD_QUIT: usize = 2;

static MAIN: AtomicIsize = AtomicIsize::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

fn hwnd(h: isize) -> HWND {
    HWND(h as *mut core::ffi::c_void)
}

/// The icon is there: the close button may hide the window.
pub fn ready() -> bool {
    READY.load(Ordering::Relaxed)
}

/// «Выход» was chosen in the icon's menu: the next close request is real.
pub fn quitting() -> bool {
    QUIT.load(Ordering::Relaxed)
}

/// Hides the main window (its taskbar button goes with it).
pub fn hide(main: isize) {
    unsafe {
        let _ = ShowWindow(hwnd(main), SW_HIDE);
    }
}

pub fn hidden(main: isize) -> bool {
    main != 0 && unsafe { !IsWindowVisible(hwnd(main)).as_bool() }
}

/// Brings the main window back; `front` also makes it the active window (a click on the icon), otherwise it only
/// appears (something needs an answer: the taskbar button will flash).
pub fn reveal(main: isize, front: bool) {
    if main == 0 {
        return;
    }
    unsafe {
        let h = hwnd(main);
        if front {
            let _ = ShowWindow(h, if IsIconic(h).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(h);
        } else if !IsWindowVisible(h).as_bool() {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
    }
    if let Some(wake) = WAKE.get() {
        wake();
    }
}

/// A launcher is already running (maybe hidden in the tray): asks it to show itself. True = this copy can leave.
pub fn activate_existing() -> bool {
    unsafe {
        match FindWindowW(CLASS, PCWSTR::null()) {
            Ok(h) if !h.is_invalid() => PostMessageW(Some(h), WM_SHOW_ME, WPARAM(0), LPARAM(0)).is_ok(),
            _ => false,
        }
    }
}

/// Puts the icon by the clock. `wake` is called (from the icon's thread) whenever the main window has to notice
/// something: it was shown again, or «Выход» was chosen.
pub fn start(main: isize, wake: impl Fn() + Send + Sync + 'static) {
    if main == 0 || MAIN.swap(main, Ordering::Relaxed) != 0 {
        return;
    }
    let _ = WAKE.set(Box::new(wake));
    let _ = std::thread::Builder::new().name("tray".into()).spawn(run);
}

fn run() {
    unsafe {
        let Ok(inst) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst.into(), lpszClassName: CLASS, ..Default::default() };
        RegisterClassW(&wc);
        let Ok(win) = CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!("screenshare"), WINDOW_STYLE(0), 0, 0, 0, 0, None, None, Some(inst.into()), None)
        else {
            return;
        };
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: win,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon(),
            ..Default::default()
        };
        for (d, c) in nid.szTip.iter_mut().zip("screenshare".encode_utf16()) {
            *d = c;
        }
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            return;
        }
        READY.store(true, Ordering::Relaxed);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        READY.store(false, Ordering::Relaxed);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// The main window's own icon (winit has set it), or the stock one.
unsafe fn icon() -> HICON {
    let main = hwnd(MAIN.load(Ordering::Relaxed));
    for kind in [ICON_SMALL, ICON_BIG] {
        let r = SendMessageW(main, WM_GETICON, Some(WPARAM(kind as usize)), Some(LPARAM(0)));
        if r.0 != 0 {
            return HICON(r.0 as *mut core::ffi::c_void);
        }
    }
    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

/// Removes the icon (the program is closing).
pub fn stop() {
    unsafe {
        if let Ok(h) = FindWindowW(CLASS, PCWSTR::null()) {
            if !h.is_invalid() && READY.load(Ordering::Relaxed) {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

unsafe extern "system" fn proc(win: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let main = MAIN.load(Ordering::Relaxed);
    match msg {
        WM_TRAY => {
            match lp.0 as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => reveal(main, true),
                WM_RBUTTONUP => {
                    if let Ok(menu) = CreatePopupMenu() {
                        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN, w!("Открыть screenshare"));
                        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Выход"));
                        let mut pt = POINT::default();
                        let _ = GetCursorPos(&mut pt);
                        // Without this the menu does not close when the user clicks elsewhere.
                        let _ = SetForegroundWindow(win);
                        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, Some(0), win, None);
                        let _ = PostMessageW(Some(win), WM_NULL, WPARAM(0), LPARAM(0));
                        let _ = DestroyMenu(menu);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SHOW_ME => {
            reveal(main, true);
            LRESULT(0)
        }
        WM_COMMAND => {
            match wp.0 & 0xFFFF {
                CMD_OPEN => reveal(main, true),
                CMD_QUIT => {
                    QUIT.store(true, Ordering::Relaxed);
                    // A hidden window may not be running its loop: show it, then ask it to close.
                    reveal(main, false);
                    let _ = PostMessageW(Some(hwnd(main)), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            // Explorer restarted: the icon has to be added again.
            static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
            let created = *TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(w!("TaskbarCreated")));
            if created != 0 && msg == created {
                let mut nid = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: win,
                    uID: 1,
                    uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                    uCallbackMessage: WM_TRAY,
                    hIcon: icon(),
                    ..Default::default()
                };
                for (d, c) in nid.szTip.iter_mut().zip("screenshare".encode_utf16()) {
                    *d = c;
                }
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            }
            DefWindowProcW(win, msg, wp, lp)
        }
    }
}
