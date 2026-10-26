// Client side: UDP -> frame reassembly -> D3D11VA decode -> window. Audio into a jitter buffer.
use crate::audiopkt::AudioRx;
use crate::gpuconv::{self, GpuConv};
use crate::gpulogic::{self, ConvPath, GpuGuard};
use crate::net::{self, Code};
use crate::ratectl;
use crate::reassembly::{KeyRequester, Reassembler, Stats};
use crate::retx;
use crate::ui::{self, Exchange};
use crate::video::{self, Decoder, OwnedFrame, Scaler};
use ffmpeg_next::ffi::{AVFrame, AVPixelFormat};
use crate::volume::{self, Hit, Panel, VolumeCtl};
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
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

pub struct ClientOpts {
    pub exchange: Exchange,
    pub audio: bool,
    pub hw_decode: bool,
    /// Colour conversion and scaling on the processor (2 swscale threads) instead of the GPU video processor.
    pub cpu_convert: bool,
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
    /// Number of the frame this picture came from (converter threads can finish out of order).
    seq: u64,
}

/// A decoded picture on its way to a converter thread.
struct ConvJob {
    seq: u64,
    frame: OwnedFrame,
    dw: u32,
    dh: u32,
}

/// Converter threads: each picture's YUV -> RGB + scaling (20+ ms on a big window) runs on one of these, so
/// the decoder isn't held up by it and two pictures convert at once.
const CONVERTERS: usize = 2;

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
    /// Opus audio datagrams found missing / expected (per-type loss for the status line).
    audio_lost: AtomicU64,
    audio_expected: AtomicU64,
    audio_requested: AtomicU64,
    /// Frames the host has sent so far (from the frame numbers seen), frames that arrived complete, frames decoded
    /// (shown or not): the three steps between "host sent N fps" and "viewer shows M fps".
    frames_sent: AtomicU64,
    frames_complete: AtomicU64,
    frames_decoded_all: AtomicU64,
    /// Video size and size of the picture after fitting it into the window, (w << 32) | h.
    video_size: AtomicU64,
    shown_size: AtomicU64,
    /// Window client size, (w << 32) | h.
    win_size: AtomicU64,
    picture: Mutex<Picture>,
    /// Frames waiting for the decoder (the channel can't tell us).
    qdepth: AtomicUsize,
    /// 0 = CPU decoder, 1 = GPU (D3D11VA), 2 = GPU requested but FFmpeg fell back to its CPU decoder,
    /// 3 = GPU decoder failed at run time and we switched to the CPU.
    dec_mode: AtomicU8,
    /// ConvPath code: where colour conversion and scaling run right now.
    conv_mode: AtomicU8,
    /// The decoder asks the receiver for a fresh keyframe (after switching decoders).
    want_resync: AtomicBool,
    /// Frames lost because the decoder queue was full / decoded but not shown while catching up.
    dropped_full: AtomicU64,
    dropped_catchup: AtomicU64,
    /// Decoded but not converted because both converter threads were busy.
    dropped_busy: AtomicU64,
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

fn size_str(v: u64) -> String {
    format!("{}x{}", v >> 32, v & 0xffff_ffff)
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
        audio_lost: AtomicU64::new(0),
        audio_expected: AtomicU64::new(0),
        audio_requested: AtomicU64::new(0),
        frames_sent: AtomicU64::new(0),
        frames_complete: AtomicU64::new(0),
        frames_decoded_all: AtomicU64::new(0),
        video_size: AtomicU64::new(0),
        shown_size: AtomicU64::new(0),
        win_size: AtomicU64::new((1280u64 << 32) | 720),
        picture: Mutex::new(Picture { w: 0, h: 0, px: Vec::new(), flash: false, seq: 0 }),
        qdepth: AtomicUsize::new(0),
        dec_mode: AtomicU8::new(0),
        conv_mode: AtomicU8::new(0),
        want_resync: AtomicBool::new(false),
        dropped_full: AtomicU64::new(0),
        dropped_catchup: AtomicU64::new(0),
        dropped_busy: AtomicU64::new(0),
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
    let diag_q = audio_q.clone();
    let audio_on = audio_q.is_some();
    {
        let (r, s) = net::socket_buffers(&sock);
        log_line(&format!("буферы сокета: приём {} КБ, отправка {} КБ", r / 1024, s / 1024));
    }

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
            let mut arx3 = if audio_q.is_some() { crate::opusaudio::Rx::new().ok() } else { None };
            let (mut fid_base, mut fid_hi): (Option<u32>, u32) = (None, 0);
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
                                    match fid_base {
                                        None => {
                                            fid_base = Some(fid);
                                            fid_hi = fid;
                                        }
                                        Some(_) if fid.wrapping_sub(fid_hi) < (1 << 31) => fid_hi = fid,
                                        _ => {}
                                    }
                                    sh.frames_sent.store(fid_hi.wrapping_sub(fid_base.unwrap_or(fid_hi)) as u64 + 1, Ordering::Relaxed);
                                    re.on_fragment(Instant::now(), fid, idx, count, body[8] & 1 != 0, &body[9..], &mut ready);
                                }
                                // 0x92: redundant-copy audio (v0.5.1+); 0x91: the old format, no repair possible.
                                net::PT_AUDIO2 => {
                                    if let Some(q) = &audio_q {
                                        arx.on_packet(body, |s, rate| q.push(s, rate));
                                    }
                                }
                                net::PT_AUDIO3 => {
                                    if let (Some(q), Some(rx3)) = (&audio_q, arx3.as_mut()) {
                                        rx3.on_datagram(Instant::now(), body, &mut |s, rate| q.push(s, rate));
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
                if let (Some(q), Some(rx3)) = (&audio_q, arx3.as_mut()) {
                    rx3.set_rtt(rtt);
                    rx3.tick(now, &mut |s, rate| q.push(s, rate));
                    let want = rx3.nacks(now);
                    if !want.is_empty() {
                        let _ = sock.send_to(&crate::opusaudio::anack_packet(token, &want), peer);
                    }
                }
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
                    sh.frames_complete.fetch_add(1, Ordering::Relaxed);
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
                let a3 = arx3.as_ref().map(|r| r.stats).unwrap_or_default();
                sh.audio_received.store(arx.stats.received + a3.received, Ordering::Relaxed);
                sh.audio_recovered.store(arx.stats.recovered + a3.recovered, Ordering::Relaxed);
                sh.audio_concealed.store(arx.stats.concealed + a3.concealed, Ordering::Relaxed);
                sh.audio_lost.store(a3.lost_datagrams, Ordering::Relaxed);
                sh.audio_expected.store(a3.expected_datagrams, Ordering::Relaxed);
                sh.audio_requested.store(a3.requested, Ordering::Relaxed);
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
            // Totals at the previous status line: sent / complete / decoded frames, video fragments, audio datagrams.
            let (mut sent0, mut comp0, mut dec0, mut vrx0, mut vmiss0, mut alost0, mut aexp0) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
            let (mut au0, mut arec0, mut aconc0) = (crate::playout::PlayStats::default(), 0u64, 0u64);
            let mut keyreq = KeyRequester::new();
            // Network lamp, judged once a second from this side's own counters.
            let mut nq = crate::netq::NetQuality::new(crate::netq::Side::Viewer);
            let (mut nq_rx0, mut nq_miss0, mut nq_res0) = (0u64, 0u64, 0u64);
            let mut nq_emit = Instant::now() - Duration::from_secs(10);
            ui::net(&nq);
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
                if audio_on && tick % 10 == 1 {
                    // Tell the host which audio formats we understand (it only sends Opus to viewers that say so).
                    net::write_header(&mut out, net::PT_CAPS, token);
                    out.push(net::CAP_OPUS);
                    let _ = sock.send_to(&out, peer);
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
                    ratectl::encode_report_rtt(&st, sh.rtt_us.load(Ordering::Relaxed), &mut out);
                    let _ = sock.send_to(&out, peer);
                }
                if tick % 10 == 0 {
                    let (rx, miss, res) =
                        (sh.rx_received.load(Ordering::Relaxed), sh.rx_missed.load(Ordering::Relaxed), sh.rx_residual.load(Ordering::Relaxed));
                    let (drx, dmiss, dres) = (rx.saturating_sub(nq_rx0), miss.saturating_sub(nq_miss0), res.saturating_sub(nq_res0));
                    (nq_rx0, nq_miss0, nq_res0) = (rx, miss, res);
                    let total = (drx + dmiss).max(1) as f64;
                    let rtt = sh.rtt_us.load(Ordering::Relaxed);
                    let sample = crate::netq::Sample {
                        dt_ms: 1000,
                        loss: dmiss as f64 / total,
                        residual: dres as f64 / total,
                        rtt_ms: (rtt > 0).then(|| rtt as f64 / 1000.0),
                        ..Default::default()
                    };
                    if drx + dmiss > 0 && (nq.update(sample).is_some() || nq_emit.elapsed() > Duration::from_secs(2)) {
                        nq_emit = Instant::now();
                        ui::net(&nq);
                    }
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
                    let (sent, comp, decall) = (
                        sh.frames_sent.load(Ordering::Relaxed),
                        sh.frames_complete.load(Ordering::Relaxed),
                        sh.frames_decoded_all.load(Ordering::Relaxed),
                    );
                    let (alost, aexp) = (sh.audio_lost.load(Ordering::Relaxed), sh.audio_expected.load(Ordering::Relaxed));
                    let pct = |a: u64, b: u64| a as f64 * 100.0 / b.max(1) as f64;
                    ui::stat(&format!(
                        "{:.0} кадров/с показано (хост отправил {:.0} → дошло целиком {:.0} → декодировано {:.0}) · {:.1} Мбит/с · пинг {:.0} мс · потери за 5 с: видео {:.1}% / звук {:.1}% (с начала: видео {:.1}%, докачано {}, ключевых запросов {}) · звук: восст. {}, заглушено {}, запрошено {}",
                        (f - f0) as f64 / secs,
                        (sent - sent0) as f64 / secs,
                        (comp - comp0) as f64 / secs,
                        (decall - dec0) as f64 / secs,
                        (b - b0) as f64 * 8.0 / secs / 1e6,
                        sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0,
                        pct(miss - vmiss0, (rx - vrx0) + (miss - vmiss0)),
                        pct(alost - alost0, aexp - aexp0),
                        pct(miss, rx + miss),
                        rec,
                        sh.keyreqs.load(Ordering::Relaxed),
                        sh.audio_recovered.load(Ordering::Relaxed),
                        sh.audio_concealed.load(Ordering::Relaxed),
                        sh.audio_requested.load(Ordering::Relaxed),
                    ));
                    (sent0, comp0, dec0, vrx0, vmiss0, alost0, aexp0) = (sent, comp, decall, rx, miss, alost, aexp);
                    let fps = (f - f0) as f64 / secs;
                    let (dec_ms, dec_max, _) = d.decode.take();
                    let (conv_ms, conv_max, _) = d.convert.take();
                    let (pres_ms, pres_max, _) = d.present.take();
                    let (q_avg, q_max, _) = d.queue.take();
                    let (jit, gap, stalls) = d.jitter.take();
                    drop(d);
                    let mode = sh.dec_mode.load(Ordering::Relaxed) as usize;
                    let diag = format!(
                        "Декодер: {} · цвет+масштаб на: {} · картинка {} → {} · декод {:.1}/{:.0} мс · цвет+масштаб {:.1}/{:.0} мс · показ {:.1}/{:.0} мс · очередь {:.1}/{:.0} кадр. · пропущено: догоняя {}, переполнение {}, конвертер занят {} · разброс кадров {:.1} мс, макс. пауза {:.0} мс, пауз >0,1 с: {}",
                        DEC_NAMES[mode.min(3)],
                        ConvPath::from_code(sh.conv_mode.load(Ordering::Relaxed)).name(),
                        size_str(sh.video_size.load(Ordering::Relaxed)),
                        size_str(sh.shown_size.load(Ordering::Relaxed)),
                        dec_ms, dec_max, conv_ms, conv_max, pres_ms, pres_max, q_avg, q_max,
                        sh.dropped_catchup.load(Ordering::Relaxed),
                        sh.dropped_full.load(Ordering::Relaxed),
                        sh.dropped_busy.load(Ordering::Relaxed),
                        jit, gap, stalls,
                    );
                    ui::diag(&diag);
                    log_line(&format!("[{}] {:.0} к/с · {}", sh.now_ms() / 1000, fps, diag));
                    if let Some(q) = &diag_q {
                        // Audio health, for finding the cause of crackle (see README: "Если звук трещит").
                        let a = q.stats();
                        let (conc, rec) = (sh.audio_concealed.load(Ordering::Relaxed), sh.audio_recovered.load(Ordering::Relaxed));
                        log_line(&format!(
                            "[{}] звук: буфер {:.0} мс (цель {:.0}) · опустошений +{} (всего {}) · переполнений +{} · тишины {} мс · дрейф {:+.0} ppm · вывод {} Гц, колбэк до {} кадр. · потерянных пакетов: восст. +{}, заглушено +{} (всего {}/{})",
                            sh.now_ms() / 1000, a.level_ms, a.target_ms,
                            a.underruns - au0.underruns, a.underruns,
                            a.trims - au0.trims,
                            (a.starved_frames - au0.starved_frames) * 1000 / a.out_rate.max(1) as u64,
                            a.drift_ppm, a.out_rate, a.cb_frames,
                            rec - arec0, conc - aconc0, rec, conc,
                        ));
                        au0 = a;
                        arec0 = rec;
                        aconc0 = conc;
                    }
                    let short = match mode {
                        1 => "GPU",
                        0 => "CPU",
                        _ => "CPU (GPU не работает)",
                    };
                    let _ = proxy.send_event(UserEv::Title(format!(
                        "screenshare — {short} · {fps:.0} к/с · декод {dec_ms:.1} мс · пинг {:.0} мс · {} — F11: полный экран",
                        sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0,
                        nq.text()
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
        let cpu_convert = o.cpu_convert;
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
            let gov = Arc::new(Mutex::new(CostGovernor::new()));
            // Pictures are converted on converter threads, except when a test wants every picture in order
            // (screenshot after N frames, A/V sync beacon).
            let pooled = frames_limit.is_none() && !crate::avsync::enabled();
            let busy = Arc::new(AtomicUsize::new(0));
            let mut seq = 0u64;
            let job_tx = {
                let (tx, rx) = mpsc::sync_channel::<ConvJob>(1);
                let rx = Arc::new(Mutex::new(rx));
                if pooled {
                    for _ in 0..CONVERTERS {
                        let (rx, sh, gov, busy, proxy) = (rx.clone(), sh.clone(), gov.clone(), busy.clone(), proxy.clone());
                        std::thread::spawn(move || {
                            let mut scaler = Scaler::new();
                            let mut px: Vec<u32> = Vec::new();
                            loop {
                                let job = match rx.lock().unwrap().recv() {
                                    Ok(j) => j,
                                    Err(_) => break,
                                };
                                let tc = Instant::now();
                                scaler.fast = gov.lock().unwrap().fast;
                                if scaler.convert(job.frame.get(), job.dw, job.dh, &mut px).is_ok() {
                                    let us = tc.elapsed().as_micros() as u64;
                                    gov.lock().unwrap().feed(us as f64 / 1000.0);
                                    sh.diag.lock().unwrap().convert.add(us);
                                    sh.shown_size.store(((job.dw as u64) << 32) | job.dh as u64, Ordering::Relaxed);
                                    let fresh = {
                                        let mut p = sh.picture.lock().unwrap();
                                        let fresh = job.seq >= p.seq;
                                        if fresh {
                                            p.seq = job.seq;
                                            p.w = job.dw;
                                            p.h = job.dh;
                                            std::mem::swap(&mut p.px, &mut px);
                                        }
                                        fresh
                                    };
                                    if fresh {
                                        let _ = proxy.send_event(UserEv::Frame);
                                        sh.frames_decoded.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                busy.fetch_sub(1, Ordering::Relaxed);
                            }
                        });
                    }
                }
                tx
            };
            // GPU colour conversion: only when every picture needn't be shown in order (same condition as the
            // converter pool) and not forced off. Any failure turns it off for good and the CPU pool takes over.
            let forced_cpu = cpu_convert || gpulogic::env_forces_cpu(std::env::var(gpulogic::FORCE_CPU_ENV).ok().as_deref());
            let mut guard = GpuGuard::new(forced_cpu, pooled);
            let mut gpu: Option<GpuConv> = None;
            sh.conv_mode.store(guard.path().code(), Ordering::Relaxed);
            {
                let msg = match guard.path() {
                    ConvPath::CpuForced => "Цвет и масштаб: процессор, 2 потока (принудительно: --cpu-convert или SS_CPU_CONVERT)".to_string(),
                    ConvPath::Gpu if dec.hardware => "Цвет и масштаб: видеокарта (D3D11 video processor); при любой ошибке перейду на процессор".to_string(),
                    _ => "Цвет и масштаб: процессор, 2 потока".to_string(),
                };
                println!("{msg}");
                log_line(&msg);
            }
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
                //
                // Lip sync: sound sits in its jitter buffer for a while, so every frame is held just as long
                // after its arrival. That keeps several frames queued, which is normal and not "behind": a frame
                // counts as late only when it is already past its own due time.
                let hold = sync_q.as_ref().map_or(Duration::ZERO, |q| q.delay().saturating_sub(DECODE_TIME).min(MAX_SYNC_HOLD));
                let due = arrived + hold;
                let now = Instant::now();
                let show = viewperf::should_show(waiting, now.saturating_duration_since(due));
                if !show {
                    sh.dropped_catchup.fetch_add(1, Ordering::Relaxed);
                }
                if due > now {
                    std::thread::sleep(due - now);
                }
                if done {
                    break;
                }
                let t0 = Instant::now();
                let mut conv_us = 0u64;
                if !guard.active() {
                    gpu = None; // release the D3D objects once the GPU path is off
                }
                dec.hw_passthrough = guard.active();
                let r = dec.decode(&data, show, |f| {
                    sh.video_size.store(((f.width.max(0) as u64) << 32) | f.height.max(0) as u64, Ordering::Relaxed);
                    let ws = sh.win_size.load(Ordering::Relaxed);
                    let (ww, wh) = ((ws >> 32) as u32, ws as u32);
                    if ww == 0 || wh == 0 || f.width <= 0 || f.height <= 0 {
                        return;
                    }
                    let (fw, fh) = (f.width as u32, f.height as u32);
                    let s = (ww as f64 / fw as f64).min(wh as f64 / fh as f64);
                    let (dw, dh) = (((fw as f64 * s) as u32).max(1), ((fh as f64 * s) as u32).max(1));
                    let hwf = f.format == AVPixelFormat::AV_PIX_FMT_D3D11 as i32;
                    let mut gpu_done = false;
                    // A tiny window (minimised, being dragged very small) isn't worth the video processor and some
                    // drivers refuse such sizes: that picture goes the CPU way, without counting as a GPU failure.
                    if hwf && guard.active() && dw >= 64 && dh >= 64 {
                        let before = guard.path();
                        let tc = Instant::now();
                        match gpuconv::step(&mut gpu, &mut guard, &mut scaler, f, dw, dh, &mut px) {
                            Ok(diff) => {
                                gpu_done = true; // `px` is right even if the check just turned the GPU path off
                                conv_us = tc.elapsed().as_micros() as u64;
                                if let Some(d) = diff {
                                    log_line(&format!("[{}] сверка видеокарта/процессор: расхождение {d:.1} из 255 (допустимо до {})", sh.now_ms() / 1000, gpulogic::MAX_MEAN_DIFF));
                                }
                            }
                            Err(e) => {
                                guard.fail(&format!("{e:#}"));
                            }
                        }
                        if guard.path() != before {
                            let msg = format!(
                                "Цвет и масштаб: видеокарта отказала ({}) — перехожу на процессор, 2 потока",
                                guard.reason().unwrap_or("?")
                            );
                            println!("{msg}");
                            log_line(&format!("[{}] {msg}", sh.now_ms() / 1000));
                        }
                    }
                    // What the label says: GPU only while hardware frames really go through it.
                    let shown_path = if gpu_done || (hwf && guard.active() && dw >= 64 && dh >= 64) {
                        guard.path()
                    } else if !hwf && guard.path() == ConvPath::Gpu {
                        ConvPath::Cpu // the decoder gave ordinary frames in memory: nothing for the video processor
                    } else {
                        guard.path()
                    };
                    sh.conv_mode.store(shown_path.code(), Ordering::Relaxed);
                    let mut cpu_owned: Option<OwnedFrame> = None;
                    if !gpu_done && pooled {
                        // Hand the picture to a converter thread; if both are busy this one isn't shown.
                        if busy.load(Ordering::Relaxed) > CONVERTERS {
                            sh.dropped_busy.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        let owned = if hwf { video::readback(f) } else { OwnedFrame::copy_of(f) };
                        if let Ok(frame) = owned {
                            seq += 1;
                            busy.fetch_add(1, Ordering::Relaxed);
                            if job_tx.try_send(ConvJob { seq, frame, dw, dh }).is_err() {
                                busy.fetch_sub(1, Ordering::Relaxed);
                                sh.dropped_busy.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        return;
                    }
                    if hwf && !gpu_done {
                        cpu_owned = video::readback(f).ok();
                        if cpu_owned.is_none() {
                            return;
                        }
                    }
                    let src: &AVFrame = cpu_owned.as_ref().map_or(f, |o| o.get());
                    if !gpu_done {
                        let tc = Instant::now();
                        scaler.fast = gov.lock().unwrap().fast;
                        if scaler.convert(src, dw, dh, &mut px).is_err() {
                            return;
                        }
                        conv_us = tc.elapsed().as_micros() as u64;
                        gov.lock().unwrap().feed(conv_us as f64 / 1000.0);
                    }
                    sh.shown_size.store(((dw as u64) << 32) | dh as u64, Ordering::Relaxed);
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
                            if full.convert(src, fw, fh, &mut img).is_ok() {
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
                if r.is_ok() {
                    sh.frames_decoded_all.fetch_add(1, Ordering::Relaxed);
                }
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
        volume: player.as_ref().map(|p| p.volume.clone()),
        overlay: None,
        cursor: (0, 0),
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
    /// Level and mute of the sound (None when there is no audio output).
    volume: Option<Arc<VolumeCtl>>,
    /// The volume panel a right click opens.
    overlay: Option<Overlay>,
    cursor: (i32, i32),
}

/// The open volume panel.
struct Overlay {
    panel: Panel,
    /// The left button is held on the slider.
    dragging: bool,
    /// Hides itself this long after the last touch.
    last_touch: Instant,
}

const OVERLAY_HIDE: Duration = Duration::from_secs(4);

impl App {
    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn open_overlay(&mut self) {
        let (Some(w), Some(_)) = (&self.window, &self.volume) else { return };
        let size = w.inner_size();
        let s = w.scale_factor().round() as i32;
        let panel = Panel::place(size.width as i32, size.height as i32, self.cursor.0, self.cursor.1, s);
        self.overlay = Some(Overlay { panel, dragging: false, last_touch: Instant::now() });
        w.set_cursor_visible(true);
        w.request_redraw();
    }

    fn close_overlay(&mut self) {
        if self.overlay.take().is_some() {
            if let Some(v) = &self.volume {
                volume::save(v);
            }
            if let Some(w) = &self.window {
                w.set_cursor_visible(w.fullscreen().is_none());
                w.request_redraw();
            }
        }
    }

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
            .with_title("screenshare — F11 или двойной клик: полный экран / окно · правая кнопка: громкость")
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
                    Key::Named(NamedKey::Escape) if self.overlay.is_some() => self.close_overlay(),
                    Key::Named(NamedKey::Escape) => {
                        if self.window.as_ref().is_some_and(|w| w.fullscreen().is_some()) {
                            self.toggle_fullscreen();
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as i32, position.y as i32);
                if let (Some(ov), Some(v)) = (&mut self.overlay, &self.volume) {
                    if ov.dragging {
                        v.set_level(ov.panel.level_at(self.cursor.0));
                        v.set_muted(false);
                        ov.last_touch = Instant::now();
                        self.redraw();
                    }
                }
            }
            // Right click: the volume panel (the left button keeps its double-click-for-fullscreen job).
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                if self.overlay.is_some() {
                    self.close_overlay();
                } else {
                    self.open_overlay();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                if let Some(ov) = &mut self.overlay {
                    if ov.dragging {
                        ov.dragging = false;
                        if let Some(v) = &self.volume {
                            volume::save(v);
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.overlay.is_some() => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0) as f32,
                };
                if let (Some(ov), Some(v)) = (&mut self.overlay, &self.volume) {
                    v.set_level((v.level() + dy * 0.05).max(0.0));
                    v.set_muted(false);
                    ov.last_touch = Instant::now();
                    volume::save(v);
                }
                self.redraw();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. }
                if self.overlay.is_some() =>
            {
                let hit = self.overlay.as_ref().map_or(Hit::Outside, |o| o.panel.hit(self.cursor.0, self.cursor.1));
                match hit {
                    Hit::Outside => self.close_overlay(),
                    Hit::Mute => {
                        if let Some(v) = &self.volume {
                            v.toggle_mute();
                            volume::save(v);
                        }
                    }
                    Hit::Slider(level) => {
                        if let Some(v) = &self.volume {
                            v.set_level(level);
                            v.set_muted(false);
                        }
                        if let Some(o) = &mut self.overlay {
                            o.dragging = true;
                        }
                    }
                    Hit::Inside => {}
                }
                if let Some(o) = &mut self.overlay {
                    o.last_touch = Instant::now();
                }
                self.last_click = None;
                self.redraw();
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

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // The panel hides itself a few seconds after the last touch (a still picture sends no frames to wake us).
        match &self.overlay {
            Some(o) if !o.dragging => {
                let deadline = o.last_touch + OVERLAY_HIDE;
                if Instant::now() >= deadline {
                    self.close_overlay();
                    el.set_control_flow(ControlFlow::Wait);
                } else {
                    el.set_control_flow(ControlFlow::WaitUntil(deadline));
                }
            }
            _ => el.set_control_flow(ControlFlow::Wait),
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
        if let (Some(o), Some(v)) = (&self.overlay, &self.volume) {
            volume::draw_panel(&mut buf, ww as i32, wh as i32, &o.panel, v.level(), v.muted());
        }
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
