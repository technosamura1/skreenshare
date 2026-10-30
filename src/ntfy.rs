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
    // Windows curl (Schannel) often dies on ntfy.sh with
    // "curl: (35) Recv failure: Connection was reset": HTTP/2, a dead IPv6 route, or a
    // revocation check that never answers. HTTP/1.1 + IPv4 + no revocation check still
    // verifies the certificate. A reset is retried; an HTTP error is not.
    let mut last = String::new();
    for attempt in 0..3 {
        let out = Command::new(curl())
            .args(["-sS", "-f", "--http1.1", "--ipv4", "--ssl-no-revoke", "-m", "20"])
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .context("не удалось запустить curl.exe")?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if attempt == 2 || !transient(&last) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(400 * (attempt as u64 + 1)));
    }
    bail!("сервер подключения недоступен: {last}");
}

fn transient(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("(35)")
        || e.contains("(56)")
        || e.contains("(28)")
        || e.contains("(7)")
        || e.contains("connection was reset")
        || e.contains("schannel")
        || e.contains("timed out")
}

/// `since` for an ntfy poll: `all`, or a message id. Nothing else, so it cannot rewrite the URL.
fn since_token(s: &str) -> String {
    let t: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(80).collect();
    if t.is_empty() { "all".into() } else { t }
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

/// Raw ntfy ndjson for `topic` since `since` (`all` or a message id from the previous poll).
/// Friends parse it with `friends::parse_relay` and keep the id as the next cursor.
pub fn poll_since(topic: &str, since: &str) -> Result<String> {
    let since = since_token(since);
    run(&[&format!("{}/{topic}/json?poll=1&since={since}", base())])
}
