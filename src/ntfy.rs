//! The HTTP side of "Простое подключение (beta)": post/poll a topic on an ntfy server (default https://ntfy-me.com,
//! no account; override with the SCREENSHARE_RELAY environment variable, e.g. a self-hosted ntfy).
//! Uses curl.exe, which ships with Windows 10 (1803+) / 11, so no HTTP/TLS stack has to be linked in.
//! NOT covered by the Linux simulation tests (needs the real network and Windows).
use anyhow::{bail, Context, Result};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

static RATE_LIMIT: Mutex<(Option<Instant>, u32)> = Mutex::new((None, 0));

pub fn retry_after_seconds() -> u64 {
    cooldown_seconds(RATE_LIMIT.lock().unwrap().0, Instant::now())
}

fn cooldown_seconds(until: Option<Instant>, now: Instant) -> u64 {
    until.filter(|t| *t > now).map(|t| (t - now).as_secs().saturating_add(1)).unwrap_or(0)
}

fn rate_error(seconds: u64) -> anyhow::Error {
    anyhow::anyhow!("Сервер подключения ограничил частоту запросов (429). Повторите через {seconds} с. Доставка не подтверждена.")
}

fn http_response(output: &str) -> Result<(u16, &str)> {
    let (body, code) = output.rsplit_once("\nSS_HTTP:").context("сервер не вернул HTTP-статус")?;
    Ok((code.trim().parse().context("неверный HTTP-статус")?, body))
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn base() -> String {
    std::env::var("SCREENSHARE_RELAY")
        .ok()
        .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
        .unwrap_or_else(|| "https://ntfy-me.com".into())
        .trim_end_matches('/')
        .to_string()
}

/// Message IDs are local to a relay. Never reuse cursors from another server.
pub fn cursor_dir() -> std::path::PathBuf {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(base().as_bytes());
    let key: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    crate::friends::profile_dir().join("relay-state").join(key)
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
    let remaining = retry_after_seconds();
    if remaining > 0 { return Err(rate_error(remaining)); }
    let mut last = String::new();
    for attempt in 0..3 {
        let out = Command::new(curl())
            .args(["-sS", "-w", "\nSS_HTTP:%{http_code}", "--http1.1", "--ipv4", "--ssl-no-revoke", "-m", "20"])
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .context("не удалось запустить curl.exe")?;
        if out.status.success() {
            let output = String::from_utf8_lossy(&out.stdout);
            let (status, body) = http_response(&output)?;
            if status == 429 {
                let mut limit = RATE_LIMIT.lock().unwrap();
                limit.1 = limit.1.saturating_add(1).min(5);
                let delay = (60u64 << (limit.1 - 1)).min(900);
                limit.0 = Some(Instant::now() + Duration::from_secs(delay));
                return Err(rate_error(delay));
            }
            if !(200..300).contains(&status) { bail!("сервер подключения недоступен: HTTP {status}"); }
            // A parallel successful request must not cancel an active cooldown.
            let mut limit = RATE_LIMIT.lock().unwrap();
            if limit.0.is_none_or(|t| t <= Instant::now()) { *limit = (None, 0); }
            return Ok(body.to_owned());
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cooldown_expires() {
        let now = Instant::now();
        assert_eq!(cooldown_seconds(None, now), 0);
        assert_eq!(cooldown_seconds(Some(now), now), 0);
        assert_eq!(cooldown_seconds(Some(now - Duration::from_secs(1)), now), 0);
        assert!(cooldown_seconds(Some(now + Duration::from_secs(60)), now) >= 60);
    }
    #[test]
    fn rate_limit_stops_subsequent_network_calls() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/test", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = [0; 4096]; let _ = socket.read(&mut buf).unwrap();
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let err = run(&[&url]).unwrap_err().to_string();
        server.join().unwrap();
        assert!(err.contains("429"));
        assert!(retry_after_seconds() > 0);
        // Unreachable URL would yield a curl connection error if cooldown were bypassed.
        assert!(run(&["http://127.0.0.1:1/no-network"]).unwrap_err().to_string().contains("429"));
        *RATE_LIMIT.lock().unwrap() = (None, 0);
    }
    #[test]
    fn http_status_is_separate_from_body() {
        assert_eq!(http_response("{\"error\":\"limited\"}\nSS_HTTP:429").unwrap(), (429, "{\"error\":\"limited\"}"));
        assert_eq!(http_response("line1\nline2\nSS_HTTP:200").unwrap(), (200, "line1\nline2"));
        assert!(http_response("missing status").is_err());
    }
}
