// Skreenshare проектор — sender for Windows: streams the screen and the system sound to the
// "Skreenshare проектор" Android app over the local network. No codes, no accounts.
#![windows_subsystem = "windows"]

mod audio;
mod bundle;
mod capture;
mod discovery;
mod gui;
mod stream;
mod video;
mod wincap;

fn main() {
    unsafe {
        // Real pixel sizes for the monitor list and capture instead of DPI-scaled ones.
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        // 1 ms timer resolution for frame pacing and the packet pacer.
        windows::Win32::Media::timeBeginPeriod(1);
    }
    // Unpack FFmpeg in the background so "Транслировать" starts instantly.
    std::thread::spawn(|| {
        let _ = video::init();
    });
    if let Err(e) = gui::run() {
        eprintln!("{e}");
    }
}
