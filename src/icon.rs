// The logo as a window icon (title bar, taskbar, Alt+Tab). The exe icon in Explorer comes from assets/icon.res,
// linked in by build.rs. Both are drawn by tools/make-icon.py.

pub const SIZE: u32 = 128;
static RGBA: &[u8] = include_bytes!("../assets/icon128.rgba");

/// Raw RGBA of the logo, `SIZE` x `SIZE`.
pub fn rgba() -> &'static [u8] {
    RGBA
}

pub fn window_icon() -> Option<winit::window::Icon> {
    winit::window::Icon::from_rgba(RGBA.to_vec(), SIZE, SIZE).ok()
}
