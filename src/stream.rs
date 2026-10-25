// One streaming session: capture -> NVENC H.264 -> paced UDP to the projector, plus system audio, plus a
// small control thread (packet re-requests, keyframe requests, keep-alive, receiver reports).
//
// Loss handling (v0.4): every video packet goes into a ring buffer of the last ~300 ms; when the projector sends a
// NACK for some (frame, packet) pairs only those are re-sent (rate-limited). A whole keyframe is only produced when
// the projector says a frame could not be recovered, and never more often than every 500 ms.
// `SS_SIM_LOSS=0.05` makes this sender drop 5 % of everything it sends (video, audio, re-sends) to test all of it.
use crate::capture::{self, Capturer, Event, Source};
use crate::wincap::WindowCapturer;
use crate::video::{Encoder, EncoderConfig, Scaler};
use anyhow::{anyhow, bail, Result};
use proto as p;
use proto::loss::LossSim;
use proto::rate::{RateController, Sample};
use proto::retx::{RetxBuffer, RetxLimiter};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Shown when this Windows cannot hide the yellow capture border of a shared window (no `IsBorderRequired`).
const BORDER_NOTE: &str = "Жёлтую рамку Windows вокруг окна тут не убрать (нужна Windows 11) — для показа без рамки выберите «Весь экран».";

#[derive(Clone, Debug)]
pub struct Settings {
    /// What to show: a monitor (None = primary) or one window.
    pub source: Source,
    pub fps: u32,
    /// Bitrate (Mbit/s): fixed value, or with `adaptive` the starting point and the ceiling (it drops on loss and recovers slowly).
    pub bitrate_mbps: u32,
    /// Lower the bitrate when the receiver reports loss, raise it slowly again when it is clean.
    pub adaptive: bool,
    /// Shifts the sound relative to the picture (ms, + = sound later) to trim lip-sync for a particular projector.
    pub av_offset_ms: i32,
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
    /// Something the user should know that is not an error (e.g. "the window was closed, showing the whole screen").
    pub notice: String,
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
    report_seq: AtomicU64,
    /// Current encoder target, kbit/s (the paced sender follows it).
    target_kbps: AtomicU32,
    /// Features both sides agreed on in the handshake (`proto::FEAT_*`).
    features: AtomicU8,
    /// The audio goes out as Opus (negotiated and the encoder started).
    opus_on: AtomicBool,
    /// Last ~300 ms of video packets, for NACK re-sends.
    retx: Mutex<RetxBuffer>,
    retx_limit: Mutex<RetxLimiter>,
    /// Test aid: drops a share of everything sent (env `SS_SIM_LOSS`, off by default).
    sim: Mutex<LossSim>,
    /// Counters (cumulative): original video packets, re-sent packets, re-sends refused by the limiter, NACK items.
    video_pkts: AtomicU64,
    retx_pkts: AtomicU64,
    retx_refused: AtomicU64,
    nack_items: AtomicU64,
    status: Mutex<Status>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn now_ms_f(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
    fn nack_on(&self) -> bool {
        self.features.load(Ordering::Relaxed) & p::FEAT_NACK != 0
    }
    /// Sends one media packet (video, audio, re-send), through the loss simulator when `SS_SIM_LOSS` is set.
    fn send_media(&self, sock: &UdpSocket, peer: SocketAddr, pkt: &[u8]) {
        self.sent_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        if self.sim.lock().unwrap().drop_next() {
            return;
        }
        let _ = sock.send_to(pkt, peer);
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
            report_seq: AtomicU64::new(0),
            target_kbps: AtomicU32::new(20_000),
            features: AtomicU8::new(0),
            opus_on: AtomicBool::new(false),
            retx: Mutex::new(RetxBuffer::new(300)),
            retx_limit: Mutex::new(RetxLimiter::new(24.0 * 1300.0)),
            sim: Mutex::new(LossSim::from_env(
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1),
            )),
            video_pkts: AtomicU64::new(0),
            retx_pkts: AtomicU64::new(0),
            retx_refused: AtomicU64::new(0),
            nack_items: AtomicU64::new(0),
            status: Mutex::new(Status {
                phase: Phase::Connecting,
                text: "Соединяюсь с проектором...".into(),
                stats: String::new(),
                audio: String::new(),
                notice: String::new(),
                error: None,
            }),
            repaint: Box::new(repaint),
        });
        let sh = shared.clone();
        let mut settings = settings;
        let thread = std::thread::spawn(move || {
            loop {
                match run_once(&sh, peer, limit, &settings) {
                    Ok(End::Resized) => continue,
                    Ok(End::SourceLost(msg)) => {
                        // The window is gone (or cannot be captured): carry on with the whole screen.
                        settings.source = Source::Monitor(None);
                        sh.set(|s| s.notice = msg);
                        continue;
                    }
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
    /// The window being shown cannot be captured any more; the message says what happens next.
    SourceLost(String),
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
    crate::video::init()?;
    let (mut cap, window_title) = match &o.source {
        Source::Window { hwnd, title } => match WindowCapturer::new(*hwnd) {
            Ok(w) => {
                if w.border == crate::wincap::BorderState::Unavailable {
                    sh.set(|s| s.notice = BORDER_NOTE.into());
                }
                (Cap::Window(w), Some(title.clone()))
            }
            Err(e) => {
                return Ok(End::SourceLost(format!("Не удалось показать окно «{}»: {e:#}. Показываю весь экран.", crate::wincap::ellipsize_title(title))));
            }
        },
        Source::Monitor(m) => {
            let monitors = capture::list_monitors()?;
            if monitors.is_empty() {
                bail!("не найден ни один монитор");
            }
            let mon = match m {
                Some(i) if *i < monitors.len() => &monitors[*i],
                _ => monitors.iter().find(|m| m.primary).unwrap_or(&monitors[0]),
            };
            (Cap::Screen(Capturer::new(mon)?), None)
        }
    };
    let (cw, ch) = cap.size();

    // ---- what to send: never more than the projector can decode
    let fps = if limit.2 > 0 { o.fps.min(limit.2) } else { o.fps }.clamp(15, 60);
    let max_w = if limit.0 > 0 { o.max_w.min(limit.0) } else { o.max_w };
    let max_h = if limit.1 > 0 { o.max_h.min(limit.1) } else { o.max_h };
    // A monitor is sent in its own proportions. A window changes size whenever the user drags its edge, but the
    // projector is told the picture size only once, so a window always gets a fixed canvas that it is letterboxed into.
    let (ow, oh) = if window_title.is_some() { (max_w.max(2) & !1, max_h.max(2) & !1) } else { fit(cw, ch, max_w, max_h) };

    // ---- audio format is part of START
    let audio_rate = if o.audio { crate::audio::default_rate().unwrap_or(0) } else { 0 };
    let start = p::Start {
        width: ow as u16,
        height: oh as u16,
        fps: fps as u8,
        vcodec: p::VCODEC_H264,
        audio_rate,
        audio_channels: 2,
        clock_sync: 1, // audio and video are stamped from the same clock (sh.start)
        // Opus only for 48 kHz (the usual rate of the Windows mixer; other rates stay raw PCM, as before).
        features: p::FEAT_NACK | p::FEAT_AUDIO_RED | if audio_rate == crate::opus::RATE { p::FEAT_OPUS } else { 0 },
    };
    sh.features.store(0, Ordering::Relaxed);
    sh.retx.lock().unwrap().clear();

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
                        if let Some((p::T_START_ACK, s, body)) = p::parse_header(&buf[..n]) {
                            if s == session {
                                // A v0.3 projector answers with an empty ACK: no NACK, no audio redundancy.
                                sh.features.store(p::parse_start_ack(body) & start.features, Ordering::Relaxed);
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
        std::thread::spawn(move || video_sender(sock, sh, peer, session, rx))
    };

    // ---- audio
    let mut _audio_stream = None;
    if audio_rate > 0 {
        let seq = AtomicU32::new(0);
        let sock = sock.clone();
        let sh2 = sh.clone();
        let mut pending: Vec<i16> = Vec::new();
        let mut pkt = Vec::with_capacity(1400);
        let feats = sh.features.load(Ordering::Relaxed);
        // Opus (v0.5) when the projector passed its decode self-test: 10 ms frames, ~128 kbit/s, every datagram carries
        // the newest frame and the three before it. If the encoder cannot start we quietly use the PCM path below.
        let mut opus_enc = if feats & p::FEAT_OPUS != 0 { crate::opus::Encoder::new(128, 0).ok() } else { None };
        let mut opus_recent: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
        let mut opus_buf: Vec<u8> = Vec::new();
        // With redundancy each packet carries two chunks, so chunks are 3 ms (two of them fit a packet easily and a
        // lost packet is covered by the next one); without it 5 ms as before.
        let red = feats & p::FEAT_AUDIO_RED != 0;
        let chunk = if opus_enc.is_some() {
            crate::opus::FRAME * 2
        } else if red {
            ((audio_rate as f64 * 0.003).round() as usize).clamp(1, 300) * 2
        } else {
            (audio_rate as usize / 200).min(280) * 2
        };
        let mut prev_chunk: Vec<u8> = Vec::new();
        let mut cur_chunk: Vec<u8> = Vec::with_capacity(chunk * 2);
        let chunk_ms = (chunk / 2) as f64 * 1000.0 / audio_rate as f64;
        let ms_per_frame = 1000.0 / audio_rate as f64;
        let offset = o.av_offset_ms as f64;
        // Capture time (ms on the stream clock) of pending[0]. Advances by sample count, so it is exact;
        // it is re-anchored on the wall clock only when it drifts or after a silent gap (loopback sends nothing then).
        let mut anchor: Option<f64> = None;
        sh.opus_on.store(opus_enc.is_some(), Ordering::Relaxed);
        match crate::audio::start_capture(move |samples, _rate| {
            let now = sh2.now_ms_f();
            let new_start = now - (samples.len() / 2) as f64 * ms_per_frame;
            let pending_ms = (pending.len() / 2) as f64 * ms_per_frame;
            match anchor {
                Some(a) if (a + pending_ms - new_start).abs() < 30.0 => {}
                _ => anchor = Some(new_start - pending_ms),
            }
            pending.extend_from_slice(samples);
            while pending.len() >= chunk {
                let a = anchor.unwrap_or(0.0);
                let ts = (a + offset).max(0.0).round() as u32;
                let n = seq.fetch_add(1, Ordering::Relaxed);
                if let Some(enc) = opus_enc.as_mut() {
                    // A silence gap in the capture re-anchors the clock; the frames before the gap keep their
                    // own spacing relative to the newest one, which is fine (the receiver treats it as a hole).
                    if enc.encode(&pending[..chunk], &mut opus_buf).is_ok() {
                        opus_recent.push_front(opus_buf.clone());
                        opus_recent.truncate(p::OPUS_DEPTH);
                        let frames: Vec<&[u8]> = opus_recent.iter().map(|v| v.as_slice()).collect();
                        p::write_audio_opus(session, n, ts, &frames, &mut pkt);
                        sh2.send_media(&sock, peer, &pkt);
                    }
                    pending.drain(..chunk);
                    anchor = Some(a + chunk_ms);
                    continue;
                }
                if red {
                    cur_chunk.clear();
                    for s in pending.drain(..chunk) {
                        cur_chunk.extend_from_slice(&s.to_le_bytes());
                    }
                    p::write_audio_red(session, n, ts, &prev_chunk, &cur_chunk, &mut pkt);
                    std::mem::swap(&mut prev_chunk, &mut cur_chunk);
                } else {
                    p::write_audio_header(session, n, ts, &mut pkt);
                    for s in pending.drain(..chunk) {
                        pkt.extend_from_slice(&s.to_le_bytes());
                    }
                }
                anchor = Some(a + chunk_ms);
                sh2.send_media(&sock, peer, &pkt);
            }
        }) {
            Ok(s) => {
                _audio_stream = Some(s);
                let how = if sh.opus_on.load(Ordering::Relaxed) { "Opus" } else { "без сжатия" };
                sh.set(|s| s.audio = format!("Звук: передаётся весь звук компьютера ({how})"));
            }
            Err(e) => sh.set(|s| s.audio = format!("Звук не захватывается: {e:#}")),
        }
    } else if o.audio {
        sh.set(|s| s.audio = "Звук не захватывается: нет устройства вывода".into());
    } else {
        sh.set(|s| s.audio = "Звук выключен".into());
    }

    // ---- capture + encode loop
    let max_mbps = o.bitrate_mbps.clamp(2, 80) as f64;
    let area = (ow as f64 * oh as f64) / (1920.0 * 1080.0);
    let min_mbps = (6.0 * area).max(2.5).min(max_mbps);
    let start_mbps = if o.adaptive { (max_mbps * area.clamp(0.45, 1.0)).clamp(min_mbps, max_mbps) } else { max_mbps };
    let rate_ctl = RateController::new(start_mbps, min_mbps, max_mbps);
    sh.target_kbps.store((start_mbps * 1000.0) as u32, Ordering::Relaxed);
    let result = capture_loop(sh, &mut cap, &tx, (cw, ch), (ow, oh), window_title.as_deref(), fps, rate_ctl, o.adaptive);
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

/// The picture source: a whole monitor or one window.
enum Cap {
    Screen(Capturer),
    Window(WindowCapturer),
}

impl Cap {
    fn size(&self) -> (u32, u32) {
        match self {
            Cap::Screen(c) => (c.width, c.height),
            Cap::Window(w) => (w.width, w.height),
        }
    }
    fn next(&mut self, timeout_ms: u32) -> Result<Event> {
        match self {
            Cap::Screen(c) => c.next(timeout_ms),
            Cap::Window(w) => w.next(timeout_ms),
        }
    }
}

/// Draws `src` (sw x sh BGRA) into the `ow` x `oh` canvas `dst`, scaled to fit and centred; the rest is black.
fn draw_letterboxed(
    scaler: &mut Scaler,
    scratch: &mut Vec<u8>,
    src: &[u8],
    (sw, sh): (u32, u32),
    dst: &mut [u8],
    stride: usize,
    (ow, oh): (u32, u32),
) -> Result<()> {
    let (x, y, fw, fh) = p::geom::letterbox_rect(sw, sh, ow, oh);
    let (x, y, fw, fh) = (x as usize, y as usize, fw as usize, fh as usize);
    let (ow, oh) = (ow as usize, oh as usize);
    // black bars
    for row in 0..oh {
        let line = &mut dst[row * stride..row * stride + ow * 4];
        if row < y || row >= y + fh {
            line.fill(0);
        } else {
            line[..x * 4].fill(0);
            line[(x + fw) * 4..].fill(0);
        }
    }
    if (fw, fh) == (sw as usize, sh as usize) {
        for row in 0..fh {
            dst[(y + row) * stride + x * 4..][..fw * 4].copy_from_slice(&src[row * sw as usize * 4..][..fw * 4]);
        }
    } else {
        scratch.resize(fw * fh * 4, 0);
        scaler.scale(src, sw, sh, sw as usize * 4, scratch, fw as u32, fh as u32, fw * 4)?;
        for row in 0..fh {
            dst[(y + row) * stride + x * 4..][..fw * 4].copy_from_slice(&scratch[row * fw * 4..][..fw * 4]);
        }
    }
    Ok(())
}

fn capture_loop(
    sh: &Arc<Shared>,
    cap: &mut Cap,
    tx: &mpsc::SyncSender<Frame>,
    (cw, ch): (u32, u32),
    (ow, oh): (u32, u32),
    window_title: Option<&str>,
    fps: u32,
    mut ctl: RateController,
    adaptive: bool,
) -> Result<End> {
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let mut enc = Encoder::new(&EncoderConfig {
        width: ow,
        height: oh,
        fps,
        bitrate: (ctl.mbps() * 1e6) as u64,
        preset: "p3".into(),
        gop: fps * 2,
    })?;
    let mut scaler = Scaler::new();
    let mut tmp: Vec<u8> = if window_title.is_none() && (cw, ch) != (ow, oh) { vec![0; (cw * ch * 4) as usize] } else { Vec::new() };
    let mut scratch: Vec<u8> = Vec::new();
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
    let mut last_adapt = Instant::now();
    let mut last_seq = 0u64;
    let mut vp_at_sample = 0u64;
    let mut vp_at_stat = 0u64;
    let mut retx_at_stat = 0u64;

    while sh.running.load(Ordering::Relaxed) {
        let now = Instant::now();
        let timeout = if dirty { next_due.saturating_duration_since(now).as_millis() as u32 } else { 100 };
        match cap.next(timeout)? {
            Event::Updated => dirty = true,
            Event::Reset => {
                if cap.size() != (cw, ch) {
                    return Ok(End::Resized);
                }
                dirty = true;
            }
            Event::Closed => {
                let t = crate::wincap::ellipsize_title(window_title.unwrap_or(""));
                return Ok(End::SourceLost(format!("Окно «{t}» закрыто. Показываю весь экран.")));
            }
            Event::Timeout => {}
        }
        // Rate-limit forced keyframes. With NACK a keyframe is only asked for when a frame could not be recovered,
        // and a keyframe is hundreds of packets: at most one per 500 ms so requests can never snowball into a loop.
        // (v0.3 projectors re-ask every ~150 ms while they wait for one: 250 ms as before.)
        let key_gap = Duration::from_millis(if sh.nack_on() { 500 } else { 250 });
        if sh.keyreq.load(Ordering::Relaxed) && last_key.elapsed() > key_gap {
            sh.keyreq.store(false, Ordering::Relaxed);
            force_key = true;
            dirty = true;
        }
        // Adaptive bitrate: once a second look at what the projector reported (also while the screen is static).
        if adaptive && last_adapt.elapsed() >= Duration::from_secs(1) {
            last_adapt = Instant::now();
            let seq = sh.report_seq.load(Ordering::Relaxed);
            let rep = sh.report.lock().unwrap().clone();
            if let (true, Some(r)) = (seq != last_seq, rep) {
                last_seq = seq;
                let rtt_ms = sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0;
                // share of the video packets that had to be asked for again in that second (0 with a v0.3 projector)
                let vp = sh.video_pkts.load(Ordering::Relaxed);
                let raw_loss = if vp > vp_at_sample { (r.nack_pkts as f64 / (vp - vp_at_sample) as f64).min(1.0) } else { 0.0 };
                vp_at_sample = vp;
                let sample = Sample { lost_frames: r.lost_frames as u32, raw_loss, dropped_frames: r.dropped_frames as u32, rtt_ms };
                if let Some(mbps) = ctl.on_sample(sample) {
                    enc.set_bitrate((mbps * 1e6) as u64, fps); // NVENC reconfigures itself (with a keyframe)
                    sh.target_kbps.store((mbps * 1000.0) as u32, Ordering::Relaxed);
                }
            }
        }
        let now = Instant::now();
        if !dirty || now < next_due {
            continue;
        }
        let t0 = Instant::now();
        let ts_ms = sh.now_ms() as u32;
        {
            let (buf, stride) = enc.input()?;
            match cap {
                Cap::Window(w) => {
                    draw_letterboxed(&mut scaler, &mut scratch, w.pixels(), (w.width, w.height), buf, stride, (ow, oh))?;
                }
                Cap::Screen(c) => {
                    if tmp.is_empty() {
                        c.compose(buf, stride);
                    } else {
                        c.compose(&mut tmp, (cw * 4) as usize);
                        scaler.scale(&tmp, cw, ch, (cw * 4) as usize, buf, ow, oh, stride)?;
                    }
                }
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
                s.text = match window_title {
                    Some(t) => format!("Идёт трансляция окна «{}» · {ow}×{oh} · {fps} к/с", crate::wincap::ellipsize_title(t)),
                    None => format!("Идёт трансляция · {ow}×{oh} · {fps} к/с"),
                };
            });
        }

        if stat_t.elapsed() >= Duration::from_secs(1) {
            let secs = stat_t.elapsed().as_secs_f64();
            let bytes = sh.sent_bytes.load(Ordering::Relaxed);
            let rtt = sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0;
            let rep = sh.report.lock().unwrap().clone();
            let mut st = format!(
                "{:.0} к/с · {:.1} Мбит/с (цель {:.0}) · кодирование {:.1} мс · пинг {:.0} мс",
                stat_frames as f64 / secs,
                (bytes - stat_bytes0) as f64 * 8.0 / secs / 1e6,
                sh.target_kbps.load(Ordering::Relaxed) as f64 / 1000.0,
                if stat_frames > 0 { stat_enc.as_secs_f64() * 1000.0 / stat_frames as f64 } else { 0.0 },
                rtt,
            );
            if let Some(r) = rep.as_ref() {
                if r.shown_fps > 0 || r.received_fps > 0 {
                    // sent -> arrived whole -> shown: where frames are lost along the way
                    st.push_str(&format!(" · отправлено {:.0} → принято {} → показано {} к/с", stat_frames as f64 / secs, r.received_fps, r.shown_fps));
                } else {
                    st.push_str(&format!(" · проектор показывает {} к/с", r.decoded_fps));
                }
                if r.lost_frames > 0 || r.dropped_frames > 0 {
                    st.push_str(&format!(" (потеряно {}, пропущено {})", r.lost_frames, r.dropped_frames));
                }
            }
            if sh.nack_on() {
                // Packet loss on the way to the projector, and how much of it was repaired by re-sending.
                let vp = sh.video_pkts.load(Ordering::Relaxed);
                let rx = sh.retx_pkts.load(Ordering::Relaxed);
                let (dvp, drx) = (vp - vp_at_stat, rx - retx_at_stat);
                vp_at_stat = vp;
                retx_at_stat = rx;
                let asked = rep.as_ref().map_or(0, |r| r.nack_pkts as u64);
                let share = if dvp > 0 { (asked as f64 / dvp as f64 * 100.0).min(100.0) } else { 0.0 };
                st.push_str(&format!(" · потери пакетов {share:.1}% (повторно отправлено {drx}/с"));
                if let Some(r) = rep.as_ref() {
                    st.push_str(&format!(", восстановлено {}", r.recovered_pkts));
                }
                st.push(')');
            }
            if sh.sim.lock().unwrap().enabled() {
                st.push_str(&format!(" · ТЕСТ: имитация потерь {:.1}%", sh.sim.lock().unwrap().rate() * 100.0));
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

fn video_sender(sock: Arc<UdpSocket>, sh: Arc<Shared>, peer: SocketAddr, session: u32, rx: mpsc::Receiver<Frame>) {
    // Smooth, not bursty: weak Wi-Fi drops packets when 100 of them arrive at once. Pace at 1.6x the encoder target
    // (room for keyframes and re-sends), with a bucket of 12 packets, never above 100 Mbit/s.
    let burst = 12.0 * 1300.0;
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
                let rate = (sh.target_kbps.load(Ordering::Relaxed) as f64 * 125.0 * 1.6).clamp(250_000.0, 12_500_000.0); // bytes/s
                let now = Instant::now();
                tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(burst);
                last = now;
                if tokens >= need || !sh.running.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1)); // timer resolution is 1 ms (timeBeginPeriod)
            }
            tokens -= need;
            if sh.nack_on() {
                sh.retx.lock().unwrap().store(sh.now_ms(), f.id, i as u16, count as u16, &pkt);
            }
            sh.video_pkts.fetch_add(1, Ordering::Relaxed);
            sh.send_media(&sock, peer, &pkt);
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
            p::T_NACK if sh.nack_on() => {
                let items = p::parse_nack(body);
                sh.nack_items.fetch_add(items.len() as u64, Ordering::Relaxed);
                let now = sh.now_ms();
                let rate = (sh.target_kbps.load(Ordering::Relaxed) as f64 * 125.0 * 0.3).max(125_000.0);
                let mut pkts = Vec::new();
                for it in items {
                    // a packet re-sent in the last 25 ms is still on its way: the receiver asked twice
                    sh.retx.lock().unwrap().collect(now, it, 25, &mut pkts);
                }
                for pkt in pkts {
                    if sh.retx_limit.lock().unwrap().allow(now, pkt.len(), rate) {
                        sh.retx_pkts.fetch_add(1, Ordering::Relaxed);
                        sh.send_media(&sock, peer, &pkt);
                    } else {
                        sh.retx_refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
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
                    sh.report_seq.fetch_add(1, Ordering::Relaxed);
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
