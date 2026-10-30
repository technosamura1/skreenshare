// ViGEmBus is NOT stored inside screenshare.exe. Defender treats an embedded driver setup as
// "this file contains a potentially unwanted program" and refuses to start the whole app.
// The button downloads the official 1.22.0 installer, checks its sha256, then runs it elevated.
// The copy in vendor/vigembus is only the pin that build.rs checks; it is not linked in.
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const INSTALLER_NAME: &str = "ViGEmBus_1.22.0_x64_x86_arm64.exe";
const INSTALLER_URL: &str = "https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe";
/// Official file from github.com/nefarius/ViGEmBus/releases/tag/v1.22.0. Same pin as build.rs.
const INSTALLER_SHA256: &str = "89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a";
const INSTALLER_LEN: u64 = 6_278_576;
static LICENSE: &str = include_str!("../vendor/vigembus/LICENSE.txt");
/// Setup switches (Advanced Installer): no window, no automatic restart. Documented by third parties
/// (winget manifests), not guaranteed by the driver's author, hence the retry with a window.
pub const SILENT_ARGS: &str = "/exenoui /qn /norestart";

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn official(data: &[u8]) -> bool {
    data.len() as u64 == INSTALLER_LEN && sha256_hex(data) == INSTALLER_SHA256
}

fn curl() -> String {
    let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let p = format!(r"{sys}\System32\curl.exe");
    if Path::new(&p).exists() { p } else { "curl".into() }
}

/// Downloads the official installer to `dest`. Refuses anything whose hash is not the pinned 1.22.0 file.
fn download(dest: &Path) -> Result<()> {
    let out = Command::new(curl())
        .args(["-fsSL", "--http1.1", "--ipv4", "--ssl-no-revoke", "-m", "120", "-o"])
        .arg(dest)
        .arg(INSTALLER_URL)
        .creation_flags(0x0800_0000)
        .output()
        .context("не удалось запустить curl.exe (он входит в Windows 10/11)")?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        bail!("не скачать установщик драйвера: {}", e.trim());
    }
    let data = std::fs::read(dest).with_context(|| format!("не прочитать {}", dest.display()))?;
    if !official(&data) {
        let _ = std::fs::remove_file(dest);
        bail!("скачанный установщик не совпал с официальным ViGEmBus 1.22.0 — файл удалён, драйвер не запускался");
    }
    Ok(())
}

/// Makes sure the official installer is on disk (already there, or downloaded now). Returns its path.
pub fn extract() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("screenshare").join("vigembus");
    std::fs::create_dir_all(&dir).with_context(|| format!("не создать {}", dir.display()))?;
    let _ = std::fs::write(dir.join("LICENSE.txt"), LICENSE);
    let path = dir.join(INSTALLER_NAME);
    if std::fs::read(&path).is_ok_and(|d| official(&d)) {
        return Ok(path);
    }
    let tmp = dir.join(format!("{INSTALLER_NAME}.{}.tmp", std::process::id()));
    download(&tmp)?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        bail!("не удалось сохранить установщик в {}", path.display());
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