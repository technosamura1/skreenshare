// Client side: UDP -> frame reassembly -> D3D11VA decode -> window. Audio into a jitter buffer.
use crate::audiopkt::AudioRx;
use crate::net::{self, Code};
use crate::ratectl;
use crate::reassembly::{KeyRequester, Reassembler, Stats};
use crate::retx;
use crate::ui::{self, Exchange};
use crate::video::{Decoder, Scaler};
use crate::viewperf::{self, Acc, CostGovernor, HwWatch, JitterMeter};
use anyhow::Result;
use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

pub struct ClientOpts {
    pub exchange: Exchange,
    pub audio: bool,
    pub hw_decode: bool,
    /// Start in a normal window instead of borderless fullscreen.
    pub windowed: bool,
    /// Test mode: save a decoded frame as BMP and quit after this many frames.
    pub screenshot: Option<PathBuf>,
    pub frames: Option<u64>,
}

/// Old (v0.5.0 and earlier) audio datagram, PT_AUDIO: seq u32 | rate u32 | n u16 | n samples | previous datagram's samples.
/// Kept so a newer viewer still plays sound from an older host. A single lost datagram is rebuilt from the next
/// one's copy; longer gaps become silence of the right length.
fn receive_legacy_audio(body: &[u8], q: &crate::audio::AudioQueue, next: &mut Option<u32>, buf: &mut Vec<i16>) {
    let seq = u32::from_le_bytes(body[0..4].try_into().unwrap());
    let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
    let n = u16::from_le_bytes([body[8], body[9]]) as usize;
    let pcm = |b: &[u8]| -> Vec<i16> { b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect() };
    let Some(cur_bytes) = body.get(10..10 + n * 2) else { return };
    let prev_bytes = &body[10 + n * 2..];
    match *next {
        Some(expect) if seq < expect => return, // late or duplicate: that slot was already filled
        Some(expect) if seq > expect => {
            let gap = (seq - expect) as usize;
            if gap <= 80 {
                // Everything but the packet right before this one is gone: fill with silence.
                buf.clear();
                buf.resize((gap - 1) * n, 0);
                if !buf.is_empty() {
                    q.push(buf, rate);
                }
                if prev_bytes.len() == n * 2 {
                    q.push(&pcm(prev_bytes), rate);
                } else {
                    buf.clear();
                    buf.resize(n, 0);
                    q.push(buf, rate);
                }
            }
        }
        _ => {}
    }
    q.push(&pcm(cur_bytes), rate);
    *next = Some(seq + 1);
}

fn send_ping(sock: &UdpSocket, peer: SocketAddr, token: u32, sh: &Shared, out: &mut Vec<u8>) {
    net::write_header(out, net::PT_PING, token);
    out.extend_from_slice(&(sh.start.elapsed().as_micros() as u64).to_le_bytes());
    let _ = sock.send_to(out, peer);
}

/// Typical decode + convert + present time; the frame hold is shortened by this much.
const DECODE_TIME: Duration = Duration::from_millis(5);
/// Never hold video longer than this, even if audio buffering balloons.
const MAX_SYNC_HOLD: Duration = Duration::from_millis(300);

struct Picture {
    w: u32,
    h: u32,
    px: Vec<u32>,
    /// Sync test: this picture is the first bright one of a beacon flash.
    flash: bool,
}

struct Shared {
    running: AtomicBool,
    waiting_key: AtomicBool,
    start: Instant,
    last_seen_ms: AtomicU64,
    rtt_us: AtomicU64,
    recv_bytes: AtomicU64,
    frames_decoded: AtomicU64,
    /// Reassembly counters (see reassembly::Stats), mirrored for the control thread.
    rx_received: AtomicU64,
    rx_missed: AtomicU64,
    rx_recovered: AtomicU64,
    rx_residual: AtomicU64,
    rx_gave_up: AtomicU64,
    keyreqs: AtomicU64,
    audio_received: AtomicU64,
    audio_recovered: AtomicU64,
    audio_concealed: AtomicU64,
    /// Window client size, (w << 32) | h.
    win_size: AtomicU64,
    picture: Mutex<Picture>,
    /// Frames waiting for the decoder (the channel can't tell us).
    qdepth: AtomicUsize,
    /// 0 = CPU decoder, 1 = GPU (D3D11VA), 2 = GPU requested but FFmpeg fell back to its CPU decoder,
    /// 3 = GPU decoder failed at run time and we switched to the CPU.
    dec_mode: AtomicU8,
    /// The decoder asks the receiver for a fresh keyframe (after switching decoders).
    want_resync: AtomicBool,
    /// Frames lost because the decoder queue was full / decoded but not shown while catching up.
    dropped_full: AtomicU64,
    dropped_catchup: AtomicU64,
    /// Per-interval timings for the diagnostics line.
    diag: Mutex<Diag>,
}

#[derive(Default)]
struct Diag {
    decode: Acc,
    convert: Acc,
    present: Acc,
    queue: Acc,
    jitter: JitterMeter,
}

const DEC_NAMES: [&str; 4] = [
    "процессор",
    "видеокарта (D3D11VA)",
    "процессор (видеокарта не потянула этот поток)",
    "процессор (видеокарта отказала, переключился)",
];

/// Windows: render/decode threads get a bit more priority so a busy PC (a game running next to the viewer)
/// doesn't make them miss frames.
fn boost_thread() {
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

fn log_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("screenshare").join("viewer.log"))
}

/// Appends a line to %LOCALAPPDATA%\screenshare\viewer.log (cut when it grows past 1 MB): something a friend
/// can send over when "it lags".
fn log_line(line: &str) {
    use std::io::Write;
    let Some(p) = log_path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{line}");
    }
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

#[derive(Debug)]
enum UserEv {
    Frame,
    Title(String),
    Exit,
}

pub fn run(o: ClientOpts) -> Result<()> {
    let sock = Arc::new(net::bind_socket()?);
    println!("Определяю адрес в интернете...");
    let nat = net::stun_discover(&sock);
    ui::report_nat(&nat);
    let host = ui::read_code(
        &o.exchange,
        net::ROLE_HOST,
        None,
        "host.code",
        "Вставь сюда код друга (начинается с H) и нажми Enter:",
        &sock,
        nat.keepalive_server,
    )?;
    let me = Code::for_socket(net::ROLE_CLIENT, host.token, &sock, &nat);
    ui::show_code(&o.exchange, &me.encode(), "client.code", "Твой код ответа — отправь его другу:")?;
    println!("Жду, пока друг вставит код ответа...");
    let peer = net::punch(&sock, net::ROLE_CLIENT, host.token, &host.cands, Duration::from_secs(300))?;
    println!("Подключено! Открываю окно просмотра.");
    ui::connected();
    view(sock, peer, host.token, o)
}

fn view(sock: Arc<UdpSocket>, peer: SocketAddr, token: u32, o: ClientOpts) -> Result<()> {
    let event_loop = EventLoop::<UserEv>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let sh = Arc::new(Shared {
        running: AtomicBool::new(true),
        waiting_key: AtomicBool::new(true),
        start: Instant::now(),
        last_seen_ms: AtomicU64::new(0),
        rtt_us: AtomicU64::new(0),
        recv_bytes: AtomicU64::new(0),
        frames_decoded: AtomicU64::new(0),
        rx_received: AtomicU64::new(0),
        rx_missed: AtomicU64::new(0),
        rx_recovered: AtomicU64::new(0),
        rx_residual: AtomicU64::new(0),
        rx_gave_up: AtomicU64::new(0),
        keyreqs: AtomicU64::new(0),
        audio_received: AtomicU64::new(0),
        audio_recovered: AtomicU64::new(0),
        audio_concealed: AtomicU64::new(0),
        win_size: AtomicU64::new((1280u64 << 32) | 720),
        picture: Mutex::new(Picture { w: 0, h: 0, px: Vec::new(), flash: false }),
        qdepth: AtomicUsize::new(0),
        dec_mode: AtomicU8::new(0),
        want_resync: AtomicBool::new(false),
        dropped_full: AtomicU64::new(0),
        dropped_catchup: AtomicU64::new(0),
        diag: Mutex::new(Diag::default()),
    });
    {
        let sh = sh.clone();
        ui::on_stop(move || sh.running.store(false, Ordering::Relaxed));
    }

    let player = if o.audio {
        match crate::audio::start_playback() {
            Ok(p) => Some(p),
            Err(e) => {
                println!("Звук не воспроизводится: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let audio_q = player.as_ref().map(|p| p.queue.clone());

    // (If the decoder falls behind, it catches up by decoding without showing, see viewperf::should_show; this
    // bound is only the last resort and costs a keyframe resync, so it is shorter than before: 0.5 s of video.)
    let (frame_tx, frame_rx) = mpsc::sync_channel::<(Vec<u8>, bool, Instant)>(30);
    let sync_q = audio_q.clone();

    // Receiver: reassembly + audio.
    {
        let sock = sock.clone();
        let sh = sh.clone();
        std::thread::spawn(move || {
            boost_thread();
            let _ = sock.set_read_timeout(Some(Duration::from_millis(10)));
            let mut buf = [0u8; 2048];
            let mut re = Reassembler::new();
            let mut ready = Vec::new();
            let mut arx = AudioRx::new();
            let mut legacy_next: Option<u32> = None;
            let mut samples: Vec<i16> = Vec::with_capacity(960);
            while sh.running.load(Ordering::Relaxed) {
                match sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if let Some((ty, body)) = net::parse_header(&buf[..n], token) {
                            sh.last_seen_ms.store(sh.now_ms(), Ordering::Relaxed);
                            sh.recv_bytes.fetch_add(n as u64, Ordering::Relaxed);
                            match ty {
                                net::PT_VIDEO if body.len() >= 9 => {
                                    let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                                    let idx = u16::from_le_bytes([body[4], body[5]]);
                                    let count = u16::from_le_bytes([body[6], body[7]]);
                                    re.on_fragment(Instant::now(), fid, idx, count, body[8] & 1 != 0, &body[9..], &mut ready);
                                }
                                // 0x92: redundant-copy audio (v0.5.1+); 0x91: the old format, no repair possible.
                                net::PT_AUDIO2 => {
                                    if let Some(q) = &audio_q {
                                        arx.on_packet(body, |s, rate| q.push(s, rate));
                                    }
                                }
                                net::PT_AUDIO if body.len() >= 10 => {
                                    if let Some(q) = &audio_q {
                                        receive_legacy_audio(body, q, &mut legacy_next, &mut samples);
                                    }
                                }
                                net::PT_PONG if body.len() >= 8 => {
                                    let t = u64::from_le_bytes(body[0..8].try_into().unwrap());
                                    let now = sh.start.elapsed().as_micros() as u64;
                                    sh.rtt_us.store(now.saturating_sub(t), Ordering::Relaxed);
                                }
                                net::PT_PUNCH => net::answer_punch(&sock, token, net::ROLE_CLIENT, body, from),
                                net::PT_BYE => {
                                    println!("Друг остановил трансляцию.");
                                    sh.running.store(false, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) if net::is_benign(&e) => {
                        // Quiet moment: release audio packets held back for reordering.
                        if let Some(q) = &audio_q {
                            arx.flush(|s, rate| q.push(s, rate));
                        }
                    }
                    Err(e) => eprintln!("сеть: {e}"),
                }
                let now = Instant::now();
                let rtt = Duration::from_micros(sh.rtt_us.load(Ordering::Relaxed));
                for (fid, idxs) in re.nacks(now, rtt) {
                    // One request per frame (a huge list is split so each datagram stays small).
                    for pkt in retx::nack_packets(token, fid, &idxs) {
                        let _ = sock.send_to(&pkt, peer);
                    }
                }
                re.check_timeout(now, rtt, &mut ready);
                if sh.want_resync.swap(false, Ordering::Relaxed) {
                    re.force_resync();
                }
                for (data, key) in ready.drain(..) {
                    sh.diag.lock().unwrap().jitter.feed(sh.start.elapsed().as_micros() as u64);
                    sh.qdepth.fetch_add(1, Ordering::Relaxed);
                    if frame_tx.try_send((data, key, Instant::now())).is_err() {
                        // Decoder fell far behind: skip ahead to a fresh keyframe.
                        sh.qdepth.fetch_sub(1, Ordering::Relaxed);
                        sh.dropped_full.fetch_add(1, Ordering::Relaxed);
                        re.force_resync();
                    }
                }
                sh.waiting_key.store(re.waiting_key, Ordering::Relaxed);
                let st = re.stats;
                sh.rx_received.store(st.received, Ordering::Relaxed);
                sh.rx_missed.store(st.missed, Ordering::Relaxed);
                sh.rx_recovered.store(st.recovered, Ordering::Relaxed);
                sh.rx_residual.store(st.residual, Ordering::Relaxed);
                sh.rx_gave_up.store(st.gave_up, Ordering::Relaxed);
                sh.audio_received.store(arx.stats.received, Ordering::Relaxed);
                sh.audio_recovered.store(arx.stats.recovered, Ordering::Relaxed);
                sh.audio_concealed.store(arx.stats.concealed, Ordering::Relaxed);
            }
        });
    }

    // Control: keyframe requests, pings, timeout, stats.
    {
        let sock = sock.clone();
        let sh = sh.clone();
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            let mut out = Vec::with_capacity(32);
            let mut tick = 0u64;
            let mut stat_t = Instant::now();
            let (mut b0, mut f0) = (0u64, 0u64);
            let mut keyreq = KeyRequester::new();
            // Ping right away: the first round-trip time sets the retry timing for lost packets.
            send_ping(&sock, peer, token, &sh, &mut out);
            while sh.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                tick += 1;
                // Keyframe request: last resort, with a backoff so a bad moment doesn't become a request storm.
                if keyreq.poll(Instant::now(), sh.waiting_key.load(Ordering::Relaxed)) {
                    net::write_header(&mut out, net::PT_KEYREQ, token);
                    let _ = sock.send_to(&out, peer);
                    sh.keyreqs.store(keyreq.sent, Ordering::Relaxed);
                }
                if tick % 5 == 0 {
                    send_ping(&sock, peer, token, &sh, &mut out);
                    // Loss report for the sender's bitrate control (totals; the host takes differences).
                    let st = Stats {
                        received: sh.rx_received.load(Ordering::Relaxed),
                        missed: sh.rx_missed.load(Ordering::Relaxed),
                        recovered: sh.rx_recovered.load(Ordering::Relaxed),
                        residual: sh.rx_residual.load(Ordering::Relaxed),
                        gave_up: sh.rx_gave_up.load(Ordering::Relaxed),
                        requested: 0,
                    };
                    ratectl::encode_report(&st, &mut out);
                    let _ = sock.send_to(&out, peer);
                }
                if sh.now_ms().saturating_sub(sh.last_seen_ms.load(Ordering::Relaxed)) > 10_000 {
                    println!("Связь потеряна.");
                    sh.running.store(false, Ordering::Relaxed);
                }
                if stat_t.elapsed() >= Duration::from_secs(5) {
                    let mut d = sh.diag.lock().unwrap();
                    let secs = stat_t.elapsed().as_secs_f64();
                    let b = sh.recv_bytes.load(Ordering::Relaxed);
                    let f = sh.frames_decoded.load(Ordering::Relaxed);
                    let (rx, miss, rec) = (
                        sh.rx_received.load(Ordering::Relaxed),
                        sh.rx_missed.load(Ordering::Relaxed),
                        sh.rx_recovered.load(Ordering::Relaxed),
                    );
                    ui::stat(&format!(
                        "{:.0} кадров/с · {:.1} Мбит/с · пинг {:.0} мс · потери сети {:.1}% (докачано {}, ключевых запросов {}) · звук: восст. {}, заглушено {}",
                        (f - f0) as f64 / secs,
                        (b - b0) as f64 * 8.0 / secs / 1e6,
                        sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0,
                        miss as f64 * 100.0 / (rx + miss).max(1) as f64,
                        rec,
                        sh.keyreqs.load(Ordering::Relaxed),
                        sh.audio_recovered.load(Ordering::Relaxed),
                        sh.audio_concealed.load(Ordering::Relaxed),
                    ));
                    let fps = (f - f0) as f64 / secs;
                    let (dec_ms, dec_max, _) = d.decode.take();
                    let (conv_ms, conv_max, _) = d.convert.take();
                    let (pres_ms, pres_max, _) = d.present.take();
                    let (q_avg, q_max, _) = d.queue.take();
                    let (jit, gap, stalls) = d.jitter.take();
                    drop(d);
                    let mode = sh.dec_mode.load(Ordering::Relaxed) as usize;
                    let diag = format!(
                        "Декодер: {} · декод {:.1}/{:.0} мс · цвет+масштаб {:.1}/{:.0} мс · показ {:.1}/{:.0} мс · очередь {:.1}/{:.0} кадр. · пропущено: догоняя {}, переполнение {} · разброс кадров {:.1} мс, макс. пауза {:.0} мс, пауз >0,1 с: {}",
                        DEC_NAMES[mode.min(3)],
                        dec_ms, dec_max, conv_ms, conv_max, pres_ms, pres_max, q_avg, q_max,
                        sh.dropped_catchup.load(Ordering::Relaxed),
                        sh.dropped_full.load(Ordering::Relaxed),
                        jit, gap, stalls,
                    );
                    ui::diag(&diag);
                    log_line(&format!("[{}] {:.0} к/с · {}", sh.now_ms() / 1000, fps, diag));
                    let short = match mode {
                        1 => "GPU",
                        0 => "CPU",
                        _ => "CPU (GPU не работает)",
                    };
                    let _ = proxy.send_event(UserEv::Title(format!(
                        "screenshare — {short} · {fps:.0} к/с · декод {dec_ms:.1} мс · пинг {:.0} мс — F11: полный экран",
                        sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0
                    )));
                    (b0, f0) = (b, f);
                    stat_t = Instant::now();
                }
            }
            let _ = proxy.send_event(UserEv::Exit);
        });
    }

    // Decoder.
    {
        let sh = sh.clone();
        let proxy = proxy.clone();
        let hw = o.hw_decode;
        let screenshot = o.screenshot.clone();
        let frames_limit = o.frames;
        std::thread::spawn(move || {
            boost_thread();
            let mut dec = match Decoder::new(hw, crate::encsel::Codec::Hevc) {
                Ok(d) => d,
                Err(e) => {
                    println!("Декодер не запустился: {e:#}");
                    sh.running.store(false, Ordering::Relaxed);
                    return;
                }
            };
            sh.dec_mode.store(dec.hardware as u8, Ordering::Relaxed);
            println!("Декодирование: {}", if dec.hardware { "видеокарта (D3D11VA)" } else { "процессор" });
            let mut scaler = Scaler::new();
            let mut full = Scaler::new();
            let mut gov = CostGovernor::new();
            let mut watch = HwWatch::new();
            let mut gpu_failed = false;
            let mut errs = 0u32;
            let mut px: Vec<u32> = Vec::new();
            let mut done = false;
            let mut flash = crate::avsync::FlashEdge::default();
            for (data, key, arrived) in frame_rx {
                let waiting = sh.qdepth.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
                // The host fell back from the video card (HEVC) to the processor (H.264) or back: the keyframe's
                // parameter sets tell which codec this stream is, and the decoder follows.
                if key {
                    if let Some(kind) = crate::encsel::detect_codec(&data) {
                        if kind != dec.codec {
                            match Decoder::new(hw && !gpu_failed, kind) {
                                Ok(d) => {
                                    println!("Поток теперь {kind:?} — переключаю декодер.");
                                    dec = d;
                                    watch = HwWatch::new();
                                    sh.dec_mode.store(dec.hardware as u8, Ordering::Relaxed);
                                }
                                Err(e) => println!("Декодер {kind:?} не запустился: {e:#}"),
                            }
                        }
                    }
                }
                // Behind by a few frames: still decode (P-frames need it) but skip the readback, colour
                // conversion and drawing. The picture on screen then jumps to the newest frame instead of
                // lagging by the whole backlog.
                let show = viewperf::should_show(waiting);
                if !show {
                    sh.dropped_catchup.fetch_add(1, Ordering::Relaxed);
                }
                // Lip sync: sound sits in its jitter buffer for a while, so hold each frame just as long
                // (only when we're not already behind: a backlog is late enough).
                if let (true, Some(q)) = (waiting == 0, &sync_q) {
                    let hold = q.delay().saturating_sub(DECODE_TIME).min(MAX_SYNC_HOLD);
                    let due = arrived + hold;
                    let now = Instant::now();
                    if due > now {
                        std::thread::sleep(due - now);
                    }
                }
                if done {
                    break;
                }
                let t0 = Instant::now();
                let mut conv_us = 0u64;
                let r = dec.decode(&data, show, |f| {
                    let ws = sh.win_size.load(Ordering::Relaxed);
                    let (ww, wh) = ((ws >> 32) as u32, ws as u32);
                    if ww == 0 || wh == 0 || f.width <= 0 || f.height <= 0 {
                        return;
                    }
                    let (fw, fh) = (f.width as u32, f.height as u32);
                    let s = (ww as f64 / fw as f64).min(wh as f64 / fh as f64);
                    let (dw, dh) = (((fw as f64 * s) as u32).max(1), ((fh as f64 * s) as u32).max(1));
                    let tc = Instant::now();
                    scaler.fast = gov.fast;
                    if scaler.convert(f, dw, dh, &mut px).is_err() {
                        return;
                    }
                    conv_us = tc.elapsed().as_micros() as u64;
                    gov.feed(conv_us as f64 / 1000.0);
                    let is_flash = crate::avsync::enabled() && {
                        let bytes = unsafe { std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4) };
                        flash.rising(crate::avsync::luma(bytes, dw as usize * 4, dw as usize, dh as usize))
                    };
                    {
                        let mut p = sh.picture.lock().unwrap();
                        p.flash |= is_flash;
                        p.w = dw;
                        p.h = dh;
                        std::mem::swap(&mut p.px, &mut px);
                    }
                    let _ = proxy.send_event(UserEv::Frame);
                    let n = sh.frames_decoded.fetch_add(1, Ordering::Relaxed) + 1;
                    if frames_limit.is_some_and(|l| n >= l) && !done {
                        if let Some(path) = &screenshot {
                            let mut img = Vec::new();
                            if full.convert(f, fw, fh, &mut img).is_ok() {
                                match write_bmp(path, fw, fh, &img) {
                                    Ok(()) => println!("Снимок сохранён: {}", path.display()),
                                    Err(e) => println!("Снимок не сохранён: {e}"),
                                }
                            }
                        }
                        done = true;
                        sh.running.store(false, Ordering::Relaxed);
                    }
                });
                let total = t0.elapsed().as_micros() as u64;
                {
                    let mut d = sh.diag.lock().unwrap();
                    d.decode.add(total.saturating_sub(conv_us));
                    if conv_us > 0 {
                        d.convert.add(conv_us);
                    }
                    d.queue.add(waiting as u64 * 1000); // as "ms" in the Acc: frames, shown with the same scale
                }
                let (ok, pictures) = match &r {
                    Ok(n) => (true, *n),
                    Err(e) => {
                        errs += 1;
                        if errs <= 5 || errs % 100 == 0 {
                            eprintln!("декодер: {e:#}");
                        }
                        (false, 0)
                    }
                };
                if dec.hardware && !gpu_failed {
                    // FFmpeg may have quietly used its CPU decoder although the GPU device exists.
                    if pictures > 0 && !dec.hw_active && sh.dec_mode.load(Ordering::Relaxed) == 1 {
                        sh.dec_mode.store(2, Ordering::Relaxed);
                        println!("Видеокарта не смогла декодировать этот поток — работает процессор.");
                    }
                    if watch.on_packet(ok, pictures) && sh.dec_mode.load(Ordering::Relaxed) != 2 {
                        gpu_failed = true;
                        match Decoder::new(false, dec.codec) {
                            Ok(d) => {
                                println!("Декодер видеокарты не работает — перехожу на процессор.");
                                dec = d;
                                sh.dec_mode.store(3, Ordering::Relaxed);
                                sh.want_resync.store(true, Ordering::Relaxed); // the new decoder needs a keyframe
                            }
                            Err(e) => eprintln!("декодер процессора: {e:#}"),
                        }
                    }
                }
            }
        });
    }

    let mut app = App {
        window: None,
        surface: None,
        sh: sh.clone(),
        proxy,
        start_fullscreen: !o.windowed,
        last_click: None,
    };
    event_loop.run_app(&mut app)?;

    sh.running.store(false, Ordering::Relaxed);
    let mut out = Vec::new();
    net::write_header(&mut out, net::PT_BYE, token);
    for _ in 0..3 {
        let _ = sock.send_to(&out, peer);
    }
    drop(player);
    Ok(())
}

struct App {
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    sh: Arc<Shared>,
    #[allow(dead_code)]
    proxy: EventLoopProxy<UserEv>,
    start_fullscreen: bool,
    last_click: Option<Instant>,
}

impl App {
    fn toggle_fullscreen(&self) {
        let Some(w) = &self.window else { return };
        let fs = w.fullscreen().is_none();
        w.set_fullscreen(fs.then_some(Fullscreen::Borderless(None)));
        // The stream already shows the host's cursor; ours on top would just be in the way.
        w.set_cursor_visible(!fs);
    }
}

impl ApplicationHandler<UserEv> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes()
            .with_title("screenshare — F11 или двойной клик: полный экран / окно")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        if self.start_fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        window.set_cursor_visible(!self.start_fullscreen);
        window.focus_window();
        let ctx = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&ctx, window.clone()).expect("softbuffer surface"));
        let s = window.inner_size();
        self.sh.win_size.store(((s.width as u64) << 32) | s.height as u64, Ordering::Relaxed);
        self.window = Some(window);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEv) {
        match ev {
            UserEv::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEv::Title(t) => {
                if let Some(w) = &self.window {
                    w.set_title(&t);
                }
            }
            UserEv::Exit => el.exit(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => {
                self.sh.win_size.store(((s.width as u64) << 32) | s.height as u64, Ordering::Relaxed);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat => {
                match event.logical_key {
                    Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
                    Key::Named(NamedKey::Escape) => {
                        if self.window.as_ref().is_some_and(|w| w.fullscreen().is_some()) {
                            self.toggle_fullscreen();
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let now = Instant::now();
                if self.last_click.is_some_and(|t| now.duration_since(t) < Duration::from_millis(400)) {
                    self.toggle_fullscreen();
                    self.last_click = None;
                } else {
                    self.last_click = Some(now);
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }
}

impl App {
    fn draw(&mut self) {
        let t0 = Instant::now();
        let (Some(w), Some(surface)) = (&self.window, &mut self.surface) else { return };
        let size = w.inner_size();
        let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if surface.resize(ww, wh).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        let (ww, wh) = (size.width as usize, size.height as usize);
        let mut p = self.sh.picture.lock().unwrap();
        let flash = std::mem::take(&mut p.flash);
        let (pw, ph) = (p.w as usize, p.h as usize);
        if pw == 0 || pw > ww || ph > wh || p.px.len() < pw * ph {
            buf.fill(0);
        } else {
            let (ox, oy) = ((ww - pw) / 2, (wh - ph) / 2);
            buf[..oy * ww].fill(0);
            for y in 0..ph {
                let row = &mut buf[(oy + y) * ww..(oy + y + 1) * ww];
                row[..ox].fill(0);
                row[ox..ox + pw].copy_from_slice(&p.px[y * pw..(y + 1) * pw]);
                row[ox + pw..].fill(0);
            }
            buf[(oy + ph) * ww..].fill(0);
        }
        drop(p);
        let _ = buf.present();
        self.sh.diag.lock().unwrap().present.add(t0.elapsed().as_micros() as u64);
        if flash {
            crate::avsync::log("client_video", crate::avsync::now_us());
        }
    }
}

/// 32-bit top-down BMP from 0x00RRGGBB pixels.
pub fn write_bmp(path: &std::path::Path, w: u32, h: u32, px: &[u32]) -> std::io::Result<()> {
    let data_len = w * h * 4;
    let mut f = Vec::with_capacity(54 + data_len as usize);
    f.extend_from_slice(b"BM");
    f.extend_from_slice(&(54 + data_len).to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&54u32.to_le_bytes());
    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&(w as i32).to_le_bytes());
    f.extend_from_slice(&(-(h as i32)).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    f.extend_from_slice(&[0; 4]);
    f.extend_from_slice(&data_len.to_le_bytes());
    f.extend_from_slice(&[0; 16]);
    for p in &px[..(w * h) as usize] {
        f.extend_from_slice(&p.to_le_bytes());
    }
    std::fs::write(path, f)
}
