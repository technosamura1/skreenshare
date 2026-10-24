// Console interaction: showing our code, reading the peer's code, menus.
use crate::net::{self, Code};
use anyhow::{bail, Result};
use std::io::{BufRead, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// How codes travel between the two sides: by a human (console + clipboard) or files (automated tests).
#[derive(Clone)]
pub enum Exchange {
    Console,
    Dir(PathBuf),
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
        Exchange::Console => loop {
            print!("{prompt} ");
            let _ = std::io::stdout().flush();
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
                continue;
            }
            match validate(&line) {
                Ok(c) => return Ok(c),
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
