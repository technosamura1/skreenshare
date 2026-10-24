// Host side: capture -> NVENC -> paced UDP. Audio goes straight from the WASAPI callback.
use crate::capture::{self, Capturer, Event, MonitorInfo};
use crate::net::{self, Code};
use crate::ui::{self, Exchange};
use crate::video::{Encoder, EncoderConfig};
use anyhow::{bail, Result};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct HostOpts {
    pub monitor: Option<usize>,
    pub bitrate_mbps: u32,
    pub fps: u32,
    pub preset: String,
    pub max_send_mbps: u32,
    pub audio: bool,
    pub exchange: Exchange,
}

pub fn pick_monitor(choice: Option<usize>, interactive: bool) -> Result<MonitorInfo> {
    let mut mons = capture::list_monitors()?;
    if mons.is_empty() {
        bail!("не найдено ни одного монитора");
    }
    if let Some(i) = choice {
        if i >= mons.len() {
            bail!("монитора {i} нет (есть 0..{})", mons.len() - 1);
        }
        return Ok(mons.swap_remove(i));
    }
    let default = mons.iter().position(|m| m.primary).unwrap_or(0);
    if mons.len() == 1 || !interactive {
        return Ok(mons.swap_remove(default));
    }
    println!("Какой монитор показывать?");
    for (i, m) in mons.iter().enumerate() {
        let star = if i == default { "  (основной)" } else { "" };
        println!("  [{}] {}x{} — {}{star}", i + 1, m.width, m.height, m.adapter);
    }
    let s = ui::ask(&format!("Номер (Enter = {}): ", default + 1));
    let i = s.parse::<usize>().ok().filter(|&n| n >= 1 && n <= mons.len()).map(|n| n - 1).unwrap_or(default);
    Ok(mons.swap_remove(i))
}

struct Shared {
    running: AtomicBool,
    keyreq: AtomicBool,
    start: Instant,
    last_seen_ms: AtomicU64,
    sent_bytes: AtomicU64,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

pub fn run(o: HostOpts) -> Result<()> {
    let interactive = matches!(o.exchange, Exchange::Console);
    let monitor = pick_monitor(o.monitor, interactive)?;
    println!("Показываю монитор {}x{} ({})", monitor.width, monitor.height, monitor.adapter);

    let sock = Arc::new(net::bind_socket()?);
    println!("Определяю адрес в интернете...");
    let nat = net::stun_discover(&sock);
    ui::report_nat(&nat);
    let token = net::random_token();
    let me = Code::for_socket(net::ROLE_HOST, token, &sock, &nat);
    ui::show_code(&o.exchange, &me.encode(), "host.code", "Твой код — отправь его другу:")?;
    let peer_code = ui::read_code(
        &o.exchange,
        net::ROLE_CLIENT,
        Some(token),
        "client.code",
        "Вставь сюда код ответа от друга и нажми Enter:",
        &sock,
        nat.keepalive_server,
    )?;
    println!("Соединяюсь...");
    let peer = net::punch(&sock, net::ROLE_HOST, token, &peer_code.cands, Duration::from_secs(30))?;
    println!("Подключено! Идёт трансляция. Чтобы остановить — закрой это окно.");
    stream(sock, peer, token, monitor, &o)
}

fn stream(sock: Arc<UdpSocket>, peer: SocketAddr, token: u32, monitor: MonitorInfo, o: &HostOpts) -> Result<()> {
    let sh = Arc::new(Shared {
        running: AtomicBool::new(true),
        keyreq: AtomicBool::new(true),
        start: Instant::now(),
        last_seen_ms: AtomicU64::new(0),
        sent_bytes: AtomicU64::new(0),
    });

    // Control channel from the client.
    {
        let sock = sock.clone();
        let sh = sh.clone();
        std::thread::spawn(move || {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = [0u8; 2048];
            let mut out = Vec::with_capacity(64);
            while sh.running.load(Ordering::Relaxed) {
                if sh.now_ms().saturating_sub(sh.last_seen_ms.load(Ordering::Relaxed)) > 10_000 {
                    println!("Связь с другом потеряна.");
                    sh.running.store(false, Ordering::Relaxed);
                    break;
                }
                let (n, from) = match sock.recv_from(&mut buf) {
                    Ok(x) => x,
                    Err(e) if net::is_benign(&e) => continue,
                    Err(e) => {
                        eprintln!("сеть: {e}");
                        continue;
                    }
                };
                let Some((ty, body)) = net::parse_header(&buf[..n], token) else { continue };
                sh.last_seen_ms.store(sh.now_ms(), Ordering::Relaxed);
                match ty {
                    net::PT_KEYREQ => sh.keyreq.store(true, Ordering::Relaxed),
                    net::PT_PING => {
                        net::write_header(&mut out, net::PT_PONG, token);
                        out.extend_from_slice(body);
                        let _ = sock.send_to(&out, from);
                    }
                    net::PT_PUNCH => net::answer_punch(&sock, token, net::ROLE_HOST, body, from),
                    net::PT_BYE => {
                        println!("Друг отключился.");
                        sh.running.store(false, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        });
    }

    // Paced sender: spreads each frame's datagrams so a keyframe doesn't overflow router queues.
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, bool, u32)>(6);
    let sender = {
        let sock = sock.clone();
        let sh = sh.clone();
        let rate = o.max_send_mbps as f64 * 125_000.0; // bytes per second
        std::thread::spawn(move || {
            let burst = 48.0 * 1300.0;
            let mut tokens = burst;
            let mut last = Instant::now();
            let mut pkt = Vec::with_capacity(net::VIDEO_HEADER_LEN + net::MAX_PAYLOAD);
            for (data, key, fid) in rx {
                let count = data.len().div_ceil(net::MAX_PAYLOAD).max(1);
                for (i, chunk) in data.chunks(net::MAX_PAYLOAD).enumerate() {
                    net::write_header(&mut pkt, net::PT_VIDEO, token);
                    pkt.extend_from_slice(&fid.to_le_bytes());
                    pkt.extend_from_slice(&(i as u16).to_le_bytes());
                    pkt.extend_from_slice(&(count as u16).to_le_bytes());
                    pkt.push(key as u8);
                    pkt.extend_from_slice(chunk);
                    let need = pkt.len() as f64;
                    loop {
                        let now = Instant::now();
                        tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(burst);
                        last = now;
                        if tokens >= need {
                            break;
                        }
                        if (need - tokens) / rate > 0.002 {
                            std::thread::sleep(Duration::from_millis(1));
                        } else {
                            std::thread::yield_now();
                        }
                    }
                    tokens -= need;
                    let _ = sock.send_to(&pkt, peer);
                    sh.sent_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
                }
            }
        })
    };

    let _audio = if o.audio {
        let sock = sock.clone();
        let sh = sh.clone();
        let mut pending: Vec<i16> = Vec::new();
        let mut seq = 0u32;
        let mut pkt = Vec::with_capacity(1100);
        match crate::audio::start_capture(move |samples, rate| {
            pending.extend_from_slice(samples);
            // 240 stereo frames (5 ms at 48 kHz) = 960 bytes per datagram.
            while pending.len() >= 480 {
                net::write_header(&mut pkt, net::PT_AUDIO, token);
                pkt.extend_from_slice(&seq.to_le_bytes());
                pkt.extend_from_slice(&rate.to_le_bytes());
                for s in pending.drain(..480) {
                    pkt.extend_from_slice(&s.to_le_bytes());
                }
                seq = seq.wrapping_add(1);
                let _ = sock.send_to(&pkt, peer);
                sh.sent_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
            }
        }) {
            Ok(s) => Some(s),
            Err(e) => {
                println!("Звук не захватывается: {e:#}. Продолжаю без звука.");
                None
            }
        }
    } else {
        None
    };

    let mut cap = Capturer::new(&monitor)?;
    let period = Duration::from_secs_f64(1.0 / o.fps as f64);
    let mut enc: Option<Encoder> = None;
    let mut dirty = true;
    let mut force_key = true;
    let mut next_due = Instant::now();
    let mut last_key = Instant::now() - Duration::from_secs(1);
    let mut fid = 0u32;
    let mut stat_t = Instant::now();
    let mut stat_frames = 0u32;
    let mut stat_enc = Duration::ZERO;
    let mut stat_bytes0 = 0u64;

    while sh.running.load(Ordering::Relaxed) {
        let now = Instant::now();
        let timeout = if dirty { next_due.saturating_duration_since(now).as_millis() as u32 } else { 250 };
        match cap.next(timeout)? {
            Event::Updated => dirty = true,
            Event::Reset => {
                dirty = true;
                if enc.as_ref().is_some_and(|e| e.width != cap.width || e.height != cap.height) {
                    enc = None;
                }
            }
            Event::Timeout => {}
        }
        // Rate-limit forced keyframes: the client re-asks every few hundred ms while waiting.
        if sh.keyreq.load(Ordering::Relaxed) && last_key.elapsed() > Duration::from_millis(250) {
            sh.keyreq.store(false, Ordering::Relaxed);
            force_key = true;
            dirty = true;
        }
        let now = Instant::now();
        if !dirty || now < next_due {
            continue;
        }
        if enc.is_none() {
            enc = Some(Encoder::new(&EncoderConfig {
                width: cap.width,
                height: cap.height,
                fps: o.fps,
                bitrate: o.bitrate_mbps as u64 * 1_000_000,
                preset: o.preset.clone(),
            })?);
            force_key = true;
        }
        let e = enc.as_mut().unwrap();
        let t0 = Instant::now();
        let (buf, stride) = e.input()?;
        cap.compose(buf, stride);
        let key_now = force_key;
        e.encode(key_now, |data, key| {
            if tx.try_send((data.to_vec(), key, fid)).is_ok() {
                fid = fid.wrapping_add(1);
            } else {
                // Network can't keep up: drop this frame, restart the chain with a keyframe.
                sh.keyreq.store(true, Ordering::Relaxed);
            }
        })?;
        stat_enc += t0.elapsed();
        if key_now {
            last_key = Instant::now();
        }
        force_key = false;
        dirty = false;
        next_due = if now.duration_since(next_due) > period { now + period } else { next_due + period };
        stat_frames += 1;

        if stat_t.elapsed() >= Duration::from_secs(5) {
            let secs = stat_t.elapsed().as_secs_f64();
            let bytes = sh.sent_bytes.load(Ordering::Relaxed);
            println!(
                "кадров/с: {:.0} | отправка: {:.1} Мбит/с | захват+кодирование: {:.1} мс",
                stat_frames as f64 / secs,
                (bytes - stat_bytes0) as f64 * 8.0 / secs / 1e6,
                if stat_frames > 0 { stat_enc.as_secs_f64() * 1000.0 / stat_frames as f64 } else { 0.0 },
            );
            stat_t = Instant::now();
            stat_frames = 0;
            stat_enc = Duration::ZERO;
            stat_bytes0 = bytes;
        }
    }
    drop(tx);
    let _ = sender.join();
    Ok(())
}
