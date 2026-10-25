//! The HTTP side of "Простое подключение (beta)": post/poll a topic on an ntfy server (default https://ntfy.sh,
//! no account; override with the SCREENSHARE_RELAY environment variable, e.g. a self-hosted ntfy).
//! Uses curl.exe, which ships with Windows 10 (1803+) / 11, so no HTTP/TLS stack has to be linked in.
//! NOT covered by the Linux simulation tests (needs the real network and Windows).
use anyhow::{bail, Context, Result};
use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn base() -> String {
    std::env::var("SCREENSHARE_RELAY")
        .ok()
        .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
        .unwrap_or_else(|| "https://ntfy.sh".into())
        .trim_end_matches('/')
        .to_string()
}

fn curl() -> String {
    let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let p = format!(r"{sys}\System32\curl.exe");
    if std::path::Path::new(&p).exists() {
        p
    } else {
        "curl".into()
    }
}

fn run(args: &[&str]) -> Result<String> {
    let out = Command::new(curl())
        .args(["-sS", "-f", "-m", "20"])
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("не удалось запустить curl.exe")?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        bail!("сервер подключения недоступен: {}", e.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Publishes one sealed message (base64url text) to the topic.
pub fn post(topic: &str, message: &str) -> Result<()> {
    run(&["--data-raw", message, &format!("{}/{topic}", base())]).map(|_| ())
}

/// All sealed messages currently cached on the topic (oldest first).
pub fn poll(topic: &str) -> Result<Vec<String>> {
    let body = run(&[&format!("{}/{topic}/json?poll=1&since=all", base())])?;
    Ok(crate::easy::parse_ntfy_lines(&body))
}
