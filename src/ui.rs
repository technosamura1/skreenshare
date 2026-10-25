// Console interaction: showing our code, reading the peer's code, menus.
use crate::net::{self, Code};
use anyhow::{bail, Result};
use std::io::{BufRead, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// How codes travel between the two sides: by a human (console + clipboard), files (automated tests),
/// or the GUI process that spawned us (`--ipc`: `@`-prefixed lines on stdout, codes on stdin).
#[derive(Clone)]
pub enum Exchange {
    Console,
    Dir(PathBuf),
    Ipc,
    /// Simple pairing (beta): the codes travel sealed through the relay; the GUI/console still works as a fallback.
    Relay(std::sync::Arc<crate::easy::Room>),
}

static IPC: AtomicBool = AtomicBool::new(false);

pub fn set_ipc() {
    IPC.store(true, Ordering::Relaxed);
}

pub fn ipc() -> bool {
    IPC.load(Ordering::Relaxed)
}

/// Periodic stats line: plain text in a console, `@STATS` for the GUI.
pub fn stat(text: &str) {
    if ipc() {
        println!("@STATS {text}");
    } else {
        println!("{text}");
    }
}

/// Viewer diagnostics line (decoder type, timings, queue, jitter): `@DIAG` for the GUI.
pub fn diag(text: &str) {
    if ipc() {
        println!("@DIAG {text}");
    } else {
        println!("{text}");
    }
}

/// Which encoder runs ("Кодирование: видеокарта NVENC ...") and, when the video card was skipped, why.
pub fn encoder(label: &str, note: Option<&str>) {
    if ipc() {
        println!("@ENCODER {label}");
        if let Some(n) = note {
            println!("@ENCWARN {}", n.replace('\n', " "));
        }
    } else {
        println!("{label}");
        if let Some(n) = note {
            println!("Видеокарта не используется: {n}");
        }
    }
}

pub fn audio_status(text: &str) {
    if ipc() {
        println!("@AUDIO {text}");
    } else {
        println!("{text}");
    }
}

/// What is being shown ("Показываю окно ..."), or `warn`: the chosen window was lost and the full screen is
/// shown instead (the GUI paints that line red).
pub fn source_status(text: &str, warn: bool) {
    if ipc() {
        println!("{} {text}", if warn { "@SOURCEWARN" } else { "@SOURCE" });
    } else {
        println!("{text}");
    }
}

/// What the self-preview is captioned with ("Окно: ..." / "Весь экран"); sent whenever the source changes.
pub fn source_label(label: &str) {
    if ipc() {
        println!("@PVLABEL {label}");
    }
}

/// One micro-preview picture for the GUI (a ready `@PREVIEW ...` line). Only in IPC mode.
pub fn preview_line(line: &str) {
    if ipc() {
        println!("{line}");
    }
}

static CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
/// Time of the GUI's latest "PREVIEW" heartbeat as ms + 1 (0 = never).
static PREVIEW_BEAT: AtomicU64 = AtomicU64::new(0);

fn clock_ms() -> u64 {
    CLOCK.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// True while the GUI is looking at the preview (its heartbeat is fresh). Nobody asks in a console: no pictures.
pub fn preview_wanted() -> bool {
    ipc() && crate::preview::heartbeat_active(PREVIEW_BEAT.load(Ordering::Relaxed), clock_ms())
}

pub fn connected() {
    if ipc() {
        println!("@CONNECTED");
    }
}

type StopCb = Box<dyn FnOnce() + Send>;
static STOP_CB: std::sync::Mutex<Option<StopCb>> = std::sync::Mutex::new(None);
static STOP_REQ: AtomicBool = AtomicBool::new(false);

fn request_stop() {
    STOP_REQ.store(true, Ordering::Relaxed);
    let cb = STOP_CB.lock().ok().and_then(|mut g| g.take());
    if let Some(f) = cb {
        f();
    }
}

/// A control line from the GUI (`STOP`, or the `PREVIEW` heartbeat). Returns true for STOP.
/// Also used by the relay code-wait, whose stdin reader may swallow one line after the pairing finished.
pub fn control_line(l: &str) -> bool {
    match l.trim() {
        "STOP" => {
            request_stop();
            true
        }
        "PREVIEW" => {
            PREVIEW_BEAT.store(clock_ms() + 1, Ordering::Relaxed);
            false
        }
        _ => false,
    }
}

/// In IPC mode the GUI stops us by writing STOP (or by closing our stdin); `PREVIEW` lines are its heartbeat.
pub fn on_stop(f: impl FnOnce() + Send + 'static) {
    if !ipc() {
        return;
    }
    if let Ok(mut g) = STOP_CB.lock() {
        *g = Some(Box::new(f));
    }
    if STOP_REQ.load(Ordering::Relaxed) {
        request_stop(); // STOP arrived before we were listening
    }
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if control_line(&line) => return,
                Ok(_) => {}
            }
        }
        request_stop();
    });
}

struct Finish(std::sync::Arc<AtomicBool>);
impl Drop for Finish {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub fn read_line() -> String {
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

pub fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    read_line()
}

pub fn show_code(ex: &Exchange, code: &str, file: &str, title: &str) -> Result<()> {
    match ex {
        Exchange::Console => {
            let copied = arboard::Clipboard::new().and_then(|mut c| c.set_text(code.to_string())).is_ok();
            println!();
            println!("{title}");
            println!();
            println!("    {code}");
            println!();
            if copied {
                println!("(уже скопирован в буфер обмена — просто вставь его в Telegram/Discord)");
            }
        }
        Exchange::Ipc => println!("@CODE {code}"),
        Exchange::Relay(room) => {
            // The ordinary code is still shown (GUI: under "не работает?"), the relay copy is the shortcut.
            if ipc() {
                println!("@CODE {code}");
            }
            let kind = if code.starts_with('H') { crate::easy::KIND_OFFER } else { crate::easy::KIND_ANSWER };
            let msg = room.seal(kind, code);
            let mut last = None;
            for attempt in 0..3 {
                match crate::ntfy::post(&room.topic, &msg) {
                    Ok(()) => {
                        last = None;
                        break;
                    }
                    Err(e) => {
                        last = Some(e);
                        std::thread::sleep(Duration::from_millis(800 * (attempt + 1)));
                    }
                }
            }
            match last {
                None => println!("Короткий код опубликован — жду друга..."),
                Some(e) => println!("Простое подключение не сработало ({e:#}). Используй обычные длинные коды."),
            }
        }
        Exchange::Dir(d) => {
            std::fs::create_dir_all(d)?;
            let tmp = d.join(format!("{file}.tmp"));
            std::fs::write(&tmp, code)?;
            std::fs::rename(tmp, d.join(file))?;
            println!("код записан в {}", d.join(file).display());
        }
    }
    Ok(())
}

/// Waits for the peer's code, keeping our NAT mapping alive meanwhile.
pub fn read_code(
    ex: &Exchange,
    want_role: u8,
    want_token: Option<u32>,
    file: &str,
    prompt: &str,
    sock: &UdpSocket,
    keepalive: Option<SocketAddr>,
) -> Result<Code> {
    let validate = |s: &str| -> Result<Code> {
        let c = Code::decode(s)?;
        if c.role != want_role {
            if want_role == net::ROLE_CLIENT {
                bail!("это твой собственный код — нужен код ОТВЕТА от друга (начинается с C)");
            }
            bail!("это код ответа (начинается с C) — нужен код того, кто показывает экран (начинается с H)");
        }
        if let Some(t) = want_token {
            if c.token != t {
                bail!("этот код ответа сделан для другого запуска — пусть друг вставит твой новый код и пришлёт ответ заново");
            }
        }
        if c.cands.is_empty() {
            bail!("в коде нет адресов");
        }
        Ok(c)
    };
    match ex {
        Exchange::Dir(d) => {
            let path = d.join(file);
            let mut ticks = 0u32;
            loop {
                if let Ok(s) = std::fs::read_to_string(&path) {
                    return validate(&s);
                }
                std::thread::sleep(Duration::from_millis(100));
                ticks += 1;
                if ticks % 100 == 0 {
                    net::stun_keepalive(sock, keepalive);
                }
                if ticks > 3000 {
                    bail!("не дождался файла {}", path.display());
                }
            }
        }
        Exchange::Relay(room) => {
            // Poll the relay for the peer's sealed code, and still accept a pasted code (the fallback).
            let kind = if want_role == net::ROLE_HOST { crate::easy::KIND_OFFER } else { crate::easy::KIND_ANSWER };
            let (tx, rx) = mpsc::channel();
            let finished = std::sync::Arc::new(AtomicBool::new(false));
            let fin = finished.clone();
            std::thread::spawn(move || loop {
                let l = read_line();
                if fin.load(Ordering::Relaxed) {
                    // Pairing is over; this blocked read swallowed a control line meant for on_stop.
                    if l.is_empty() {
                        request_stop();
                    } else {
                        control_line(&l);
                    }
                    break;
                }
                let eof = l.is_empty();
                if tx.send(l).is_err() || eof {
                    break;
                }
            });
            let _done = Finish(finished);
            let started = std::time::Instant::now();
            let mut attempt = 0u32;
            let mut warned = false;
            let mut last_keepalive = std::time::Instant::now();
            let mut next_poll = std::time::Instant::now();
            loop {
                if started.elapsed() > Duration::from_secs(crate::easy::ROOM_LIFETIME_SECS) {
                    bail!("короткий код устарел (15 минут) — создай новый");
                }
                if std::time::Instant::now() >= next_poll {
                    match crate::ntfy::poll(&room.topic) {
                        Ok(msgs) => {
                            warned = false;
                            if let Some(code) = room.find(kind, msgs.iter()) {
                                match validate(&code) {
                                    Ok(c) => return Ok(c),
                                    // Stale/foreign message: ignore, keep waiting (a bad one must not end the room).
                                    Err(e) => eprintln!("пропускаю сообщение из комнаты: {e:#}"),
                                }
                            }
                        }
                        Err(e) => {
                            if !warned {
                                println!("Нет связи с сервером подключения: {e:#}. Пробую ещё...");
                                warned = true;
                            }
                        }
                    }
                    next_poll = std::time::Instant::now() + Duration::from_millis(crate::easy::poll_delay_ms(attempt));
                    attempt += 1;
                }
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(l) if l.is_empty() => bail!("интерфейс закрыт"),
                    Ok(l) if l == "STOP" => bail!("остановлено"),
                    Ok(l) if l == "PREVIEW" => {}
                    Ok(l) => match validate(&l) {
                        Ok(c) => return Ok(c),
                        Err(e) if ipc() => println!("@BADCODE {e:#}"),
                        Err(e) => println!("Не подошло: {e:#}"),
                    },
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => bail!("ввод закрыт"),
                }
                if last_keepalive.elapsed() >= Duration::from_secs(10) {
                    net::stun_keepalive(sock, keepalive);
                    last_keepalive = std::time::Instant::now();
                }
            }
        }
        Exchange::Console | Exchange::Ipc => loop {
            let is_ipc = matches!(ex, Exchange::Ipc);
            if !is_ipc {
                print!("{prompt} ");
                let _ = std::io::stdout().flush();
            }
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(read_line());
            });
            let line = loop {
                match rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(l) => break l,
                    Err(mpsc::RecvTimeoutError::Timeout) => net::stun_keepalive(sock, keepalive),
                    Err(mpsc::RecvTimeoutError::Disconnected) => bail!("ввод закрыт"),
                }
            };
            if line.is_empty() {
                if is_ipc {
                    bail!("интерфейс закрыт");
                }
                continue;
            }
            match validate(&line) {
                Ok(c) => return Ok(c),
                Err(e) if is_ipc => println!("@BADCODE {e:#}"),
                Err(e) => println!("Не подошло: {e:#}"),
            }
        },
    }
}

pub fn report_nat(nat: &net::NatInfo) {
    match nat.public {
        Some(p) => println!("Адрес в интернете: {}", p.ip()),
        None => println!(
            "Не удалось узнать адрес в интернете (STUN-серверы недоступны). Сработает только в одной локальной сети."
        ),
    }
    if nat.symmetric {
        println!("Внимание: у тебя «симметричный» NAT (часто у мобильного интернета) — прямое соединение может не получиться.");
    }
}
