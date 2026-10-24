// One streaming session: capture -> NVENC H.264 -> paced UDP to the projector, plus system audio, plus a
// small control thread (keyframe requests, keep-alive, receiver reports).
use crate::capture::{self, Capturer, Event};
use crate::video::{Encoder, EncoderConfig, Scaler};
use anyhow::{anyhow, bail, Result};
use proto as p;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Settings {
    /// Index into the monitor list (None = primary).
    pub monitor: Option<usize>,
    pub fps: u32,
    pub bitrate_mbps: u32,
    /// Upper bound for the picture size (the desktop is scaled down to fit, keeping its aspect ratio).
    pub max_w: u32,
    pub max_h: u32,
    pub audio: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Connecting,
    Streaming,
    Ended,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub phase: Phase,
    pub text: String,
    pub stats: String,
    pub audio: String,
    pub error: Option<String>,
}

struct Shared {
    running: AtomicBool,
    keyreq: AtomicBool,
    start: Instant,
    last_seen_ms: AtomicU64,
    sent_bytes: AtomicU64,
    rtt_us: AtomicU32,
    report: Mutex<Option<p::Report>>,
    status: Mutex<Status>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn set(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
        (self.repaint)();
    }
}

pub struct Stream {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Stream {
    /// `limit` is what the projector reported it can decode (0 = unknown).
    pub fn start(peer: SocketAddr, limit: (u32, u32, u32), settings: Settings, repaint: impl Fn() + Send + Sync + 'static) -> Stream {
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            keyreq: AtomicBool::new(true),
            start: Instant::now(),
            last_seen_ms: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
            rtt_us: AtomicU32::new(0),
            report: Mutex::new(None),
            status: Mutex::new(Status {
                phase: Phase::Connecting,
                text: "Соединяюсь с проектором...".into(),
                stats: String::new(),
                audio: String::new(),
                error: None,
            }),
            repaint: Box::new(repaint),
        });
        let sh = shared.clone();
        let thread = std::thread::spawn(move || {
            loop {
                match run_once(&sh, peer, limit, &settings) {
                    Ok(End::Resized) => continue,
                    Ok(End::Stopped) => break,
                    Err(e) => {
                        let msg = format!("{e:#}");
                        sh.set(|s| s.error = Some(msg));
                        break;
                    }
                }
            }
            sh.running.store(false, Ordering::Relaxed);
            sh.set(|s| s.phase = Phase::Ended);
        });
        Stream { shared, thread: Some(thread) }
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        self.shared.running.store(false, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum End {
    Stopped,
    /// The desktop changed size; the projector must be told again.
    Resized,
}

fn even(v: f64) -> u32 {
    ((v.round() as u32).max(2)) & !1
}

/// Largest size with the desktop's aspect ratio that fits `max` (never upscales).
pub fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
    if s >= 1.0 {
        (w & !1, h & !1)
    } else {
        (even(w as f64 * s), even(h as f64 * s))
    }
}

fn make_socket() -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = s.set_send_buffer_size(2 << 20);
    s.bind(&SocketAddr::from(([0, 0, 0, 0], 0)).into())?;
    Ok(s.into())
}

fn run_once(sh: &Arc<Shared>, peer: SocketAddr, limit: (u32, u32, u32), o: &Settings) -> Result<End> {
    let sock = Arc::new(make_socket()?);
    let mut sid = [0u8; 4];
    getrandom::fill(&mut sid).map_err(|e| anyhow!("getrandom: {e}"))?;
    let session = u32::from_le_bytes(sid) | 1;

    // ---- screen
    let monitors = capture::list_monitors()?;
    if monitors.is_empty() {
        bail!("не найден ни один монитор");
    }
    let mon = match o.monitor {
        Some(i) if i < monitors.len() => &monitors[i],
        _ => monitors.iter().find(|m| m.primary).unwrap_or(&monitors[0]),
    };
    crate::video::init()?;
    let mut cap = Capturer::new(mon)?;
    let (cw, ch) = (cap.width, cap.height);

    // ---- what to send: never more than the projector can decode
    let fps = if limit.2 > 0 { o.fps.min(limit.2) } else { o.fps }.clamp(15, 60);
    let max_w = if limit.0 > 0 { o.max_w.min(limit.0) } else { o.max_w };
    let max_h = if limit.1 > 0 { o.max_h.min(limit.1) } else { o.max_h };
    let (ow, oh) = fit(cw, ch, max_w, max_h);

    // ---- audio format is part of START
    let audio_rate = if o.audio { crate::audio::default_rate().unwrap_or(0) } else { 0 };
    let start = p::Start {
        width: ow as u16,
        height: oh as u16,
        fps: fps as u8,
        vcodec: p::VCODEC_H264,
        audio_rate,
        audio_channels: 2,
    };

    // ---- handshake: START until START_ACK
    sh.set(|s| s.text = format!("Соединяюсь с проектором {}...", peer.ip()));
    {
        sock.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut pkt = Vec::new();
        p::write_start(session, &start, &mut pkt);
        let mut buf = [0u8; 256];
        let t0 = Instant::now();
        let mut acked = false;
        while sh.running.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(6) {
            sock.send_to(&pkt, peer)?;
            match sock.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if from.ip() == peer.ip() {
                        if let Some((p::T_START_ACK, s, _)) = p::parse_header(&buf[..n]) {
                            if s == session {
                                acked = true;
                                break;
                            }
                        }
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        if !sh.running.load(Ordering::Relaxed) {
            return Ok(End::Stopped);
        }
        if !acked {
            bail!("Проектор не отвечает. Откройте приложение «Skreenshare проектор» на проекторе и проверьте, что он в той же сети Wi-Fi.");
        }
    }
    sh.last_seen_ms.store(sh.now_ms(), Ordering::Relaxed);

    // ---- control thread
    let ctl_stop = Arc::new(AtomicBool::new(false));
    let ctl = {
        let sock = sock.clone();
        let sh = sh.clone();
        let stop = ctl_stop.clone();
        std::thread::spawn(move || control(sock, sh, stop, peer, session))
    };

    // ---- paced video sender
    let (tx, rx) = mpsc::sync_channel::<Frame>(6);
    let sender = {
        let sock = sock.clone();
        let sh = sh.clone();
        let rate = (o.bitrate_mbps as f64 * 4.0).max(40.0) * 125_000.0; // bytes per second
        std::thread::spawn(move || video_sender(sock, sh, peer, session, rate, rx))
    };

    // ---- audio
    let mut _audio_stream = None;
    if audio_rate > 0 {
        let seq = AtomicU32::new(0);
        let sock = sock.clone();
        let sh2 = sh.clone();
        let mut pending: Vec<i16> = Vec::new();
        let mut pkt = Vec::with_capacity(1200);
        let chunk = ((audio_rate / 200).min(280) * 2) as usize; // 5 ms of stereo
        match crate::audio::start_capture(move |samples, _rate| {
            pending.extend_from_slice(samples);
            while pending.len() >= chunk {
                p::write_audio_header(session, seq.fetch_add(1, Ordering::Relaxed), sh2.now_ms() as u32, &mut pkt);
                for s in pending.drain(..chunk) {
                    pkt.extend_from_slice(&s.to_le_bytes());
                }
                let _ = sock.send_to(&pkt, peer);
                sh2.sent_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
            }
        }) {
            Ok(s) => {
                _audio_stream = Some(s);
                sh.set(|s| s.audio = "Звук: передаётся весь звук компьютера".into());
            }
            Err(e) => sh.set(|s| s.audio = format!("Звук не захватывается: {e:#}")),
        }
    } else if o.audio {
        sh.set(|s| s.audio = "Звук не захватывается: нет устройства вывода".into());
    } else {
        sh.set(|s| s.audio = "Звук выключен".into());
    }

    // ---- capture + encode loop
    let result = capture_loop(sh, &mut cap, &tx, (cw, ch), (ow, oh), fps, o.bitrate_mbps);
    drop(_audio_stream);
    drop(tx);
    let _ = sender.join();
    // Say goodbye (a few times: UDP).
    let mut bye = Vec::new();
    p::write_simple(p::T_BYE, session, &mut bye);
    for _ in 0..3 {
        let _ = sock.send_to(&bye, peer);
    }
    ctl_stop.store(true, Ordering::Relaxed);
    let _ = ctl.join();
    result
}

struct Frame {
    data: Vec<u8>,
    key: bool,
    id: u32,
    ts_ms: u32,
}

fn capture_loop(
    sh: &Arc<Shared>,
    cap: &mut Capturer,
    tx: &mpsc::SyncSender<Frame>,
    (cw, ch): (u32, u32),
    (ow, oh): (u32, u32),
    fps: u32,
    bitrate_mbps: u32,
) -> Result<End> {
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let mut enc = Encoder::new(&EncoderConfig {
        width: ow,
        height: oh,
        fps,
        bitrate: bitrate_mbps as u64 * 1_000_000,
        preset: "p3".into(),
        gop: fps * 2,
    })?;
    let mut scaler = Scaler::new();
    let mut tmp: Vec<u8> = if (cw, ch) != (ow, oh) { vec![0; (cw * ch * 4) as usize] } else { Vec::new() };
    let mut dirty = true;
    let mut force_key = true;
    let mut next_due = Instant::now();
    let mut last_key = Instant::now() - Duration::from_secs(1);
    let mut fid = 0u32;
    let mut stat_t = Instant::now();
    let mut stat_frames = 0u32;
    let mut stat_enc = Duration::ZERO;
    let mut stat_bytes0 = 0u64;
    let mut started = false;

    while sh.running.load(Ordering::Relaxed) {
        let now = Instant::now();
        let timeout = if dirty { next_due.saturating_duration_since(now).as_millis() as u32 } else { 100 };
        match cap.next(timeout)? {
            Event::Updated => dirty = true,
            Event::Reset => {
                if (cap.width, cap.height) != (cw, ch) {
                    return Ok(End::Resized);
                }
                dirty = true;
            }
            Event::Timeout => {}
        }
        // Rate-limit forced keyframes: the receiver re-asks every ~150 ms while it waits for one.
        if sh.keyreq.load(Ordering::Relaxed) && last_key.elapsed() > Duration::from_millis(250) {
            sh.keyreq.store(false, Ordering::Relaxed);
            force_key = true;
            dirty = true;
        }
        let now = Instant::now();
        if !dirty || now < next_due {
            continue;
        }
        let t0 = Instant::now();
        let ts_ms = sh.now_ms() as u32;
        {
            let (buf, stride) = enc.input()?;
            if tmp.is_empty() {
                cap.compose(buf, stride);
            } else {
                cap.compose(&mut tmp, (cw * 4) as usize);
                scaler.scale(&tmp, cw, ch, (cw * 4) as usize, buf, ow, oh, stride)?;
            }
        }
        let key_now = force_key;
        enc.encode(key_now, |data, key| {
            let data = if key { p::h264::patch_access_unit(data) } else { data.to_vec() };
            let f = Frame { data, key, id: fid, ts_ms };
            if tx.try_send(f).is_ok() {
                fid = fid.wrapping_add(1);
            } else {
                // Network can't keep up: this frame is gone, so restart the chain with a keyframe.
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
        if !started {
            started = true;
            sh.set(|s| {
                s.phase = Phase::Streaming;
                s.text = format!("Идёт трансляция · {ow}×{oh} · {fps} к/с · {bitrate_mbps} Мбит/с");
            });
        }

        if stat_t.elapsed() >= Duration::from_secs(1) {
            let secs = stat_t.elapsed().as_secs_f64();
            let bytes = sh.sent_bytes.load(Ordering::Relaxed);
            let rtt = sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0;
            let rep = sh.report.lock().unwrap().clone();
            let mut st = format!(
                "{:.0} к/с · {:.1} Мбит/с · кодирование {:.1} мс · пинг {:.0} мс",
                stat_frames as f64 / secs,
                (bytes - stat_bytes0) as f64 * 8.0 / secs / 1e6,
                if stat_frames > 0 { stat_enc.as_secs_f64() * 1000.0 / stat_frames as f64 } else { 0.0 },
                rtt,
            );
            if let Some(r) = rep {
                st.push_str(&format!(" · проектор показывает {} к/с", r.decoded_fps));
                if r.lost_frames > 0 || r.dropped_frames > 0 {
                    st.push_str(&format!(" (потеряно {}, пропущено {})", r.lost_frames, r.dropped_frames));
                }
            }
            sh.set(|s| s.stats = st);
            stat_t = Instant::now();
            stat_frames = 0;
            stat_enc = Duration::ZERO;
            stat_bytes0 = bytes;
        }
    }
    Ok(End::Stopped)
}

fn video_sender(sock: Arc<UdpSocket>, sh: Arc<Shared>, peer: SocketAddr, session: u32, rate: f64, rx: mpsc::Receiver<Frame>) {
    let burst = 32.0 * 1300.0;
    let mut tokens = burst;
    let mut last = Instant::now();
    let mut pkt = Vec::with_capacity(p::VIDEO_HEADER_LEN + p::MAX_PAYLOAD);
    for f in rx {
        let count = p::fragment_count(f.data.len());
        for (i, chunk) in f.data.chunks(p::MAX_PAYLOAD).enumerate() {
            let frag = p::VideoFrag { ts_ms: f.ts_ms, frame_id: f.id, idx: i as u16, count: count as u16, key: f.key };
            p::write_video_header(session, &frag, &mut pkt);
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
}

/// Reads what the projector sends back and keeps the session alive.
fn control(sock: Arc<UdpSocket>, sh: Arc<Shared>, stop: Arc<AtomicBool>, peer: SocketAddr, session: u32) {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
    let mut buf = [0u8; 2048];
    let mut out = Vec::with_capacity(32);
    let mut last_ping = Instant::now() - Duration::from_secs(5);
    while sh.running.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        if sh.now_ms().saturating_sub(sh.last_seen_ms.load(Ordering::Relaxed)) > 5000 {
            sh.set(|s| s.error = Some("Связь с проектором потеряна (он перестал отвечать).".into()));
            sh.running.store(false, Ordering::Relaxed);
            break;
        }
        // Keep-alive: a static screen produces no video packets for a long time.
        if last_ping.elapsed() >= Duration::from_millis(500) {
            last_ping = Instant::now();
            p::write_ping(p::T_PING, session, sh.now_ms() as u32, &mut out);
            let _ = sock.send_to(&out, peer);
        }
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(_) => continue, // timeout (or ICMP reset): look at the clock again
        };
        if from.ip() != peer.ip() {
            continue;
        }
        let Some((ty, s, body)) = p::parse_header(&buf[..n]) else { continue };
        if s != session {
            continue;
        }
        sh.last_seen_ms.store(sh.now_ms(), Ordering::Relaxed);
        match ty {
            p::T_KEYREQ => sh.keyreq.store(true, Ordering::Relaxed),
            p::T_PING => {
                out.clear();
                out.extend_from_slice(&buf[..n]);
                out[1] = p::T_PONG;
                let _ = sock.send_to(&out, peer);
            }
            p::T_PONG => {
                if body.len() >= 4 {
                    let sent = u32::from_le_bytes(body[0..4].try_into().unwrap());
                    let rtt_ms = (sh.now_ms() as u32).wrapping_sub(sent);
                    if rtt_ms < 5000 {
                        sh.rtt_us.store(rtt_ms * 1000, Ordering::Relaxed);
                    }
                }
            }
            p::T_REPORT => {
                if let Some(r) = p::parse_report(body) {
                    *sh.report.lock().unwrap() = Some(r);
                }
            }
            p::T_BYE => {
                sh.set(|s| s.text = "Проектор завершил приём.".into());
                sh.running.store(false, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}
