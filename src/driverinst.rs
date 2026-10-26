// The ViGEmBus driver installer travels inside the exe (vendor/vigembus, BSD-3-Clause, see NOTICE there).
// "Установить драйвер геймпада" unpacks it to %LOCALAPPDATA%\screenshare\vigembus, starts it elevated (UAC),
// waits for it and reports the exit code; the GUI then re-checks whether the driver answers.
use anyhow::{bail, Context, Result};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const INSTALLER_NAME: &str = "ViGEmBus_1.22.0_x64_x86_arm64.exe";
static INSTALLER: &[u8] = include_bytes!("../vendor/vigembus/ViGEmBus_1.22.0_x64_x86_arm64.exe");
static LICENSE: &str = include_str!("../vendor/vigembus/LICENSE.txt");
/// Setup switches (Advanced Installer): no window, no automatic restart. Documented by third parties
/// (winget manifests), not guaranteed by the driver's author, hence the retry with a window.
pub const SILENT_ARGS: &str = "/exenoui /qn /norestart";

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// Unpacks the installer (and the driver's licence next to it); returns the path of the installer.
pub fn extract() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("screenshare").join("vigembus");
    std::fs::create_dir_all(&dir).with_context(|| format!("не создать {}", dir.display()))?;
    let _ = std::fs::write(dir.join("LICENSE.txt"), LICENSE);
    let path = dir.join(INSTALLER_NAME);
    if !std::fs::metadata(&path).is_ok_and(|m| m.len() == INSTALLER.len() as u64) {
        let tmp = dir.join(format!("{INSTALLER_NAME}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, INSTALLER).with_context(|| format!("не записать {}", tmp.display()))?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    if !path.exists() {
        bail!("не удалось распаковать установщик в {}", dir.display());
    }
    Ok(path)
}

/// Starts `exe` elevated (the Windows permission prompt) and waits for it. Returns its exit code;
/// 1223 (`ERROR_CANCELLED`) when the user declines the prompt.
pub fn run_elevated(exe: &std::path::Path, args: &str) -> Result<u32> {
    let file = wide(exe.as_os_str());
    let params = wide(std::ffi::OsStr::new(args));
    let verb = wide(std::ffi::OsStr::new("runas"));
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: if args.is_empty() { PCWSTR::null() } else { PCWSTR(params.as_ptr()) },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut sei) {
            if e.code().0 as u32 & 0xFFFF == 1223 {
                return Ok(1223);
            }
            return Err(e).context("не удалось запустить установщик");
        }
        let h = sei.hProcess;
        if h.is_invalid() {
            bail!("установщик не вернул процесс");
        }
        let w = WaitForSingleObject(h, INFINITE);
        let mut code = 0u32;
        let got = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
        if w != WAIT_OBJECT_0 || got.is_err() {
            bail!("не удалось дождаться установщика");
        }
        Ok(code)
    }
}

/// The whole job (run it on a thread, it blocks while the installer works): silent run, a second run with a
/// window if the silent one failed, then the verdict from the exit code and a fresh driver check.
pub fn install(driver_answers: impl Fn() -> bool) -> Result<crate::guilogic::DriverEnd> {
    use crate::guilogic as lg;
    let exe = extract()?;
    let mut code = run_elevated(&exe, SILENT_ARGS)?;
    if lg::retry_with_window(code) && !driver_answers() {
        code = run_elevated(&exe, "")?;
    }
    // The device can take a moment to appear after setup returns.
    let mut ok = driver_answers();
    for _ in 0..6 {
        if ok {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        ok = driver_answers();
    }
    Ok(lg::classify_driver_install(code, ok))
}
