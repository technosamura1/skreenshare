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
use crate::bar::{self, Bar, Hit};
use crate::inputview::ViewInput;
use crate::volume::{self, VolumeCtl};
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
use winit::event::{DeviceEvent, DeviceId, ElementState, StartCause, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{Fullscreen, Window, WindowId};

pub struct ClientOpts {
    /// Remote control (gamepad, mouse, keyboard to the host when its owner allows it). `--no-control` turns it off.
    pub control: bool,
    pub exchange: Exchange,
    pub audio: bool,
    pub hw_decode: bool,
    /// Colour conversion and scaling on the processor (2 swscale threads) instead of the GPU video processor.
    pub cpu_convert: bool,
    /// Start in a normal window instead of borderless fullscreen.
    pub windowed: bool,
    /// «Игра»: show frames at once and keep only a small sound cushion (F3 switches while watching).
    pub game: bool,
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

/// The loss report as its own datagram. (0.7.7–0.11.4 appended these numbers to the ping without a header:
/// the host saw only a ping, and bitrate control never learned about any loss.)
fn report_datagram(out: &mut Vec<u8>, token: u32, st: &Stats, rtt_us: u64) {
    net::write_header(out, net::PT_REPORT, token);
    ratectl::encode_report_rtt(st, rtt_us, out);
}

#[cfg(test)]
mod report_tests {
    use super::*;

    #[test]
    fn loss_report_reaches_the_hosts_tracker() {
        let st = Stats { received: 970, missed: 30, recovered: 28, residual: 2, gave_up: 0, requested: 0 };
        let mut out = vec![0xEE; 7]; // leftovers from a previous packet must not leak in
        report_datagram(&mut out, 0xABCD, &st, 16_000);
        let (ty, body) = net::parse_header(&out, 0xABCD).expect("our packet");
        assert_eq!(ty, net::PT_REPORT);
        let d = ratectl::ReportTracker::default().on_report(body).expect("a report");
        assert_eq!((d.received, d.missed, d.residual, d.rtt_us), (970, 30, 2, 16_000));
    }
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
    /// Network lamp for the player bar: 0 good, 1 weak, 2 bad, 255 not known yet.
    net_level: AtomicU8,
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
    /// «Игра»: frames are not held back for lip sync (F3 toggles).
    game: AtomicBool,
}

#[derive(Default)]
struct Diag {
    decode: Acc,
    convert: Acc,
    present: Acc,
    queue: Acc,
    jitter: JitterMeter,
    /// The same, but measured where it matters: when a new picture actually reaches the screen.
    shown: JitterMeter,
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

/// Appends a line to %LOCALAPPDATA%\screenshare\viewer.log (cut when it grows past 1 MB): something a user
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
    println!("Определяю адрес в интернете...");
    let (sock, nat) = net::open_and_discover()?;
    let sock = Arc::new(sock);
    ui::report_nat(&nat);
    let host = ui::read_code(
        &o.exchange,
        net::ROLE_HOST,
        None,
        "host.code",
        "Вставьте сюда код трансляции (начинается с H) и нажмите Enter:",
        &sock,
        nat.keepalive_server,
    )?;
    let me = Code::for_socket(net::ROLE_CLIENT, host.token, &sock, &nat);
    ui::show_code(&o.exchange, &me.encode(), "client.code", "Ваш код ответа — передайте его тому, кто показывает экран:")?;
    println!("Жду, пока код ответа введут на стороне показа...");
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
        net_level: AtomicU8::new(255),
        conv_mode: AtomicU8::new(0),
        want_resync: AtomicBool::new(false),
        dropped_full: AtomicU64::new(0),
        dropped_catchup: AtomicU64::new(0),
        dropped_busy: AtomicU64::new(0),
        diag: Mutex::new(Diag::default()),
        game: AtomicBool::new(o.game),
    });
    {
        let sh = sh.clone();
        ui::on_stop(move || sh.running.store(false, Ordering::Relaxed));
    }
    // Remote control: its own thread; it sends nothing until the "Управление" switch is on and the host allows it.
    let control = o.control;
    let input = ViewInput::new();
    // Always: the laser pointer and reactions go through the same thread (the input itself stays off).
    {
        let sh = sh.clone();
        input.spawn(sock.clone(), peer, token, move || sh.running.load(Ordering::Relaxed));
    }
    // SS_ANNOT_SCRIPT=1 (tests): the laser circles the picture and every reaction goes out once.
    if std::env::var("SS_ANNOT_SCRIPT").is_ok() {
        let (inp, sh) = (input.clone(), sh.clone());
        std::thread::spawn(move || {
            use crate::annot::Reaction;
            let t0 = Instant::now();
            let all = [Reaction::Laugh, Reaction::Heart, Reaction::Yes, Reaction::No];
            while sh.running.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(12) {
                let a = t0.elapsed().as_secs_f64();
                inp.laser(Some(((32768.0 + a.cos() * 16000.0) as u16, (32768.0 + a.sin() * 16000.0) as u16)));
                let k = (a / 2.0) as usize;
                if a > 1.0 && (a - 1.0) % 2.0 < 0.02 && k < 5 {
                    inp.react(all[(k + 3) % 4]);
                }
                std::thread::sleep(Duration::from_millis(16));
            }
            inp.laser(None);
        });
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
    if let Some(q) = &audio_q {
        q.set_game(o.game);
    }

    // (If the decoder falls behind, it catches up by decoding without showing, see viewperf::should_show; this
    // bound is only the last resort and costs a keyframe resync: 0.5 s of video at 144 fps, 1.2 s at 60.)
    let (frame_tx, frame_rx) = mpsc::sync_channel::<(Vec<u8>, bool, Instant)>(72);
    let sync_q = audio_q.clone();
    let diag_q = audio_q.clone();
    let window_q = audio_q.clone();
    let audio_on = audio_q.is_some();
    {
        let (r, s) = net::socket_buffers(&sock);
        log_line(&format!("буферы сокета: приём {} КБ, отправка {} КБ", r / 1024, s / 1024));
    }

    // Receiver: reassembly + audio.
    {
        let sock = sock.clone();
        let sh = sh.clone();
        let input = input.clone();
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
                                // Remote control answers from the host (permissions, force feedback).
                                net::PT_CTRL if control && from == peer => input.on_ctrl(body),
                                net::PT_RUMBLE if control && from == peer => input.on_rumble(body),
                                net::PT_BYE => {
                                    println!("Показ остановлен на другой стороне.");
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
                    // Timed from its first packet: a frame finished late by a retransmission keeps its place in
                    // the playback timeline (the lip-sync hold covers the repair) instead of showing up late.
                    let arrived = re.take_first_seen().unwrap_or_else(Instant::now);
                    sh.frames_complete.fetch_add(1, Ordering::Relaxed);
                    sh.diag.lock().unwrap().jitter.feed(sh.start.elapsed().as_micros() as u64);
                    sh.qdepth.fetch_add(1, Ordering::Relaxed);
                    if frame_tx.try_send((data, key, arrived)).is_err() {
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
            let mut gr_bytes0 = 0u64;
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
                if (audio_on || control) && tick % 10 == 1 {
                    // Tell the host what we understand: Opus audio (it only sends Opus to viewers that say so)
                    // and remote control (it only sends control answers to viewers that say so).
                    net::write_header(&mut out, net::PT_CAPS, token);
                    out.push(if audio_on { net::CAP_OPUS } else { 0 } | if control { net::CAP_INPUT } else { 0 });
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
                    report_datagram(&mut out, token, &st, sh.rtt_us.load(Ordering::Relaxed));
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
                    if drx + dmiss > 0 {
                        sh.net_level.store(nq.level as u8, Ordering::Relaxed);
                    }
                    // One point per second for the launcher's graph (bitrate = what arrived, ping, loss before repairs).
                    let rb = sh.recv_bytes.load(Ordering::Relaxed);
                    ui::graph(rb.saturating_sub(gr_bytes0) as f64 * 8.0 / 1e6, (rtt > 0).then(|| rtt as f64 / 1000.0), sample.loss);
                    gr_bytes0 = rb;
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
                    let (sjit, sgap, _) = d.shown.take();
                    drop(d);
                    let mode = sh.dec_mode.load(Ordering::Relaxed) as usize;
                    let diag = format!(
                        "Декодер: {} · цвет+масштаб на: {} · картинка {} → {} · декод {:.1}/{:.0} мс · цвет+масштаб {:.1}/{:.0} мс · показ {:.1}/{:.0} мс · очередь {:.1}/{:.0} кадр. · пропущено: догоняя {}, переполнение {}, конвертер занят {} · разброс кадров {:.1} мс, макс. пауза {:.0} мс, пауз >0,1 с: {} · на экране: разброс {:.1} мс, макс. пауза {:.0} мс",
                        DEC_NAMES[mode.min(3)],
                        ConvPath::from_code(sh.conv_mode.load(Ordering::Relaxed)).name(),
                        size_str(sh.video_size.load(Ordering::Relaxed)),
                        size_str(sh.shown_size.load(Ordering::Relaxed)),
                        dec_ms, dec_max, conv_ms, conv_max, pres_ms, pres_max, q_avg, q_max,
                        sh.dropped_catchup.load(Ordering::Relaxed),
                        sh.dropped_full.load(Ordering::Relaxed),
                        sh.dropped_busy.load(Ordering::Relaxed),
                        jit, gap, stalls,
                        sjit, sgap,
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
                        "screenshare — {} · {short} · {fps:.0} к/с · декод {dec_ms:.1} мс · пинг {:.0} мс · {} — F3: режим, F11: полный экран",
                        if sh.game.load(Ordering::Relaxed) { "Игра" } else { "Кино" },
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
            let mut last_arrived: Option<Instant> = None;
            let mut interval = 0.0f64; // smoothed seconds between frames
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
                // «Игра»: shown at once (sound may trail by its small cushion); «Кино»: held for lip sync.
                let hold = if sh.game.load(Ordering::Relaxed) {
                    Duration::ZERO
                } else {
                    sync_q.as_ref().map_or(Duration::ZERO, |q| q.delay().saturating_sub(DECODE_TIME).min(MAX_SYNC_HOLD))
                };
                let due = arrived + hold;
                let now = Instant::now();
                // How deep the queue normally is depends on the frame rate (30 / 60 / 144): measure it.
                if let Some(prev) = last_arrived.replace(arrived) {
                    let dt = arrived.saturating_duration_since(prev).as_secs_f64().clamp(1.0 / 240.0, 1.0 / 24.0);
                    interval = if interval == 0.0 { dt } else { interval + (dt - interval) * 0.05 };
                }
                let limit = viewperf::backlog_limit(hold, Duration::from_secs_f64(interval));
                let show = viewperf::should_show(waiting, now.saturating_duration_since(due), limit);
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
                        // Same counter as the converter threads' jobs, so the window can tell new pictures
                        // from redraws on this path too.
                        seq += 1;
                        p.seq = seq;
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
        no_audio_note: if !o.audio {
            "Звук выключен: в проверке на этом ПК он не передаётся, чтобы не было эха"
        } else {
            "Звук недоступен: не удалось открыть устройство вывода"
        },
        no_audio_at: None,
        cursor: (0, 0),
        last_move: Instant::now(),
        drag: false,
        hover: Hit::Outside,
        bar_drawn: true,
        cursor_shown: true,
        shown_seq: 0,
        relative_mode: false,
        audio_q: window_q,
        started: Instant::now(),
        control,
        input: input.clone(),
        captured: false,
        mods: ModifiersState::empty(),
        esc_since: None,
        capture_t: None,
        mouse_frac: (0.0, 0.0),
        pad_panel: false,
        laser: false,
        toast: None,
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
    /// Why there is no volume control, and when the bar's speaker/slider was last touched without one (shows the note).
    no_audio_note: &'static str,
    no_audio_at: Option<Instant>,
    cursor: (i32, i32),
    /// Last mouse movement / touch of the bar: the bar is shown for a while after it (see guilogic::bar_opacity).
    last_move: Instant,
    /// The left button is held on the volume slider.
    drag: bool,
    hover: Hit,
    /// The last picture had the bar on it (so one more redraw is needed to wipe it off when it has faded out).
    bar_drawn: bool,
    cursor_shown: bool,
    /// `Picture::seq` last put on screen (tells a new frame from a redraw of the bar).
    shown_seq: u64,
    /// Controlling with raw motion (the host's cursor is hidden) rather than by the pointer's position.
    relative_mode: bool,
    /// The sound queue, for switching «Кино» / «Игра» (F3).
    audio_q: Option<crate::audio::AudioQueue>,
    started: Instant,
    /// Remote control is enabled (no `--no-control`).
    control: bool,
    input: Arc<ViewInput>,
    /// The mouse is captured: pointer confined and hidden, mouse and keys go to the host.
    captured: bool,
    mods: ModifiersState,
    /// Esc is held (since when): one second releases the mouse.
    esc_since: Option<Instant>,
    capture_t: Option<Instant>,
    /// Sub-pixel rest of raw mouse movement.
    mouse_frac: (f64, f64),
    /// F2: the gamepad test panel.
    pad_panel: bool,
    /// L: the pointer is a laser on the host's screen.
    laser: bool,
    /// A short confirmation (a reaction went out).
    toast: Option<(String, Instant)>,
}

/// The bar fades 0.4 s after 2.6 s without mouse movement; ~30 fps of redraws only while it is visible.
const BAR_TICK: Duration = Duration::from_millis(33);

impl App {
    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn scale(&self) -> i32 {
        self.window.as_ref().map_or(1, |w| (w.scale_factor().round() as i32).max(1))
    }

    fn bar_geometry(&self) -> Option<Bar> {
        let w = self.window.as_ref()?;
        let size = w.inner_size();
        Some(Bar::with_control(size.width as i32, size.height as i32, self.scale(), self.ctl_available()))
    }

    /// 0..1: how visible the bar is right now. It stays up while the pointer is on it or the slider is dragged.
    fn bar_opacity(&self) -> f32 {
        let on_bar = self.drag || self.hover != Hit::Outside;
        if on_bar {
            return 1.0;
        }
        crate::guilogic::bar_opacity(self.last_move.elapsed().as_millis())
    }

    /// The bar was touched: show it again, and the mouse cursor with it.
    fn wake_bar(&mut self) {
        self.last_move = Instant::now();
        self.set_cursor(true);
        self.redraw();
    }

    fn set_cursor(&mut self, shown: bool) {
        if self.cursor_shown != shown {
            self.cursor_shown = shown;
            if let Some(w) = &self.window {
                w.set_cursor_visible(shown);
            }
        }
    }

    /// The host understands remote control (it has answered) and this viewer has it enabled.
    fn ctl_available(&self) -> bool {
        self.control && self.input.host_info().is_some()
    }

    fn set_capture(&mut self, on: bool) {
        if on == self.captured {
            return;
        }
        self.captured = on;
        self.input.captured.store(on, Ordering::Relaxed);
        self.esc_since = None;
        self.mouse_frac = (0.0, 0.0);
        if let Some(w) = &self.window {
            if on {
                self.capture_t = Some(Instant::now());
                self.relative_mode = !self.input.host_relative(); // force apply_pointer_mode to set things up
                self.apply_pointer_mode();
            } else {
                let _ = w.set_cursor_grab(winit::window::CursorGrabMode::None);
                w.set_cursor_visible(true);
                self.cursor_shown = true;
                self.capture_t = None;
                self.last_move = Instant::now();
            }
        }
        if !on {
            self.input.release_kbm();
        }
        self.redraw();
    }

    /// While controlling: if the host shows its cursor (desktop, browser, most windows) our pointer stays visible
    /// and the host puts its cursor exactly where ours is on the picture; if the host hid its cursor (a game took
    /// the mouse), ours hides too and raw motion turns the camera. Follows the host when that changes.
    fn apply_pointer_mode(&mut self) {
        if !self.captured {
            return;
        }
        let relative = self.input.host_relative();
        if relative == self.relative_mode {
            return;
        }
        self.relative_mode = relative;
        let Some(w) = &self.window else { return };
        use winit::window::CursorGrabMode;
        if relative {
            self.input.mouse_abs(None);
            if w.set_cursor_grab(CursorGrabMode::Locked).is_err() {
                let _ = w.set_cursor_grab(CursorGrabMode::Confined);
            }
        } else {
            let _ = w.set_cursor_grab(CursorGrabMode::None);
            if w.set_cursor_grab(CursorGrabMode::Confined).is_err() {
                let _ = w.set_cursor_grab(CursorGrabMode::Locked);
            }
        }
        w.set_cursor_visible(!relative);
        self.cursor_shown = !relative;
    }

    /// Where `pos` (window pixels) is on the picture, 0..=65535 both ways (clamped to its edges).
    fn picture_spot(&self, pos: winit::dpi::PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let w = self.window.as_ref()?;
        let size = w.inner_size();
        let (pw, ph) = {
            let p = self.sh.picture.lock().unwrap();
            (p.w as f64, p.h as f64)
        };
        if pw < 1.0 || ph < 1.0 || size.width == 0 || size.height == 0 {
            return None;
        }
        // The picture is drawn fitted and centred (black bars where the aspect differs).
        let s = (size.width as f64 / pw).min(size.height as f64 / ph);
        let (dw, dh) = (pw * s, ph * s);
        let (ox, oy) = ((size.width as f64 - dw) / 2.0, (size.height as f64 - dh) / 2.0);
        let fx = ((pos.x - ox) / dw).clamp(0.0, 1.0);
        let fy = ((pos.y - oy) / dh).clamp(0.0, 1.0);
        Some(((fx * 65535.0).round() as u16, (fy * 65535.0).round() as u16))
    }

    fn toggle_control(&mut self) {
        if !self.ctl_available() {
            return;
        }
        let on = !self.input.want.load(Ordering::Relaxed);
        self.input.want.store(on, Ordering::Relaxed);
        *self.input.revoked_at.lock().unwrap() = None;
        if !on {
            self.set_capture(false);
        }
        self.wake_bar();
    }

    /// A key of the viewer's keyboard while the mouse is captured: goes to the host as a scan code.
    fn forward_key(&mut self, ev: &winit::event::KeyEvent) {
        let down = ev.state == ElementState::Pressed;
        if matches!(ev.logical_key, Key::Named(NamedKey::Escape)) {
            self.esc_since = if down { self.esc_since.or(Some(Instant::now())) } else { None };
        }
        let Some(sc) = ev.physical_key.to_scancode() else { return };
        // Plain or 0xE0-extended scan codes only (Pause and the like are left out).
        if sc == 0 || sc > 0xFFFF || !matches!(sc >> 8, 0 | 0xE0) || sc & 0xFF == 0 {
            return;
        }
        self.input.key(sc as u16, down, ev.repeat);
    }

    /// L (laser) and 1-4 (reactions) by their place on the keyboard, whatever the layout. True: handled.
    fn annot_key(&mut self, ev: &winit::event::KeyEvent) -> bool {
        use crate::annot::Reaction;
        use winit::keyboard::{KeyCode, PhysicalKey};
        let PhysicalKey::Code(k) = ev.physical_key else { return false };
        let r = match k {
            KeyCode::KeyL => {
                self.laser = !self.laser;
                let spot = if self.laser { self.picture_spot(winit::dpi::PhysicalPosition::new(self.cursor.0 as f64, self.cursor.1 as f64)) } else { None };
                self.input.laser(spot);
                self.redraw();
                return true;
            }
            KeyCode::Digit1 | KeyCode::Numpad1 => Reaction::Laugh,
            KeyCode::Digit2 | KeyCode::Numpad2 => Reaction::Heart,
            KeyCode::Digit3 | KeyCode::Numpad3 => Reaction::Yes,
            KeyCode::Digit4 | KeyCode::Numpad4 => Reaction::No,
            _ => return false,
        };
        self.input.react(r);
        self.toast = Some((format!("Отправлено: {}", r.name()), Instant::now()));
        self.redraw();
        true
    }

    /// The line at the top: a reaction just sent, the laser, remote control, or the first-minute tip.
    fn hint(&self) -> Option<(String, u32, f32)> {
        const LASER: u32 = 0x00_FF_40_40;
        if let Some((t, at)) = &self.toast {
            if at.elapsed() < Duration::from_millis(1500) {
                return Some((t.clone(), 0x00_3E_CF_8E, 1.0));
            }
        }
        if self.laser && !self.captured {
            return Some(("Лазерная указка у показывающего на экране · L — выключить · 1 смех · 2 сердечко · 3 ДА · 4 НЕТ".into(), LASER, 0.85));
        }
        if let Some(h) = self.ctl_hint() {
            return Some(h);
        }
        let t = self.started.elapsed();
        (t > Duration::from_secs(2) && t < Duration::from_secs(12))
            .then(|| ("L — лазерная указка · 1–4 — реакции (смех, сердечко, ДА, НЕТ)".into(), LASER, 0.85))
    }

    /// What to tell the viewer about remote control right now (a short line, a dot colour).
    fn ctl_hint(&self) -> Option<(String, u32, f32)> {
        const RED: u32 = 0x00_F0_5A_5A;
        const YELLOW: u32 = 0x00_F0_B4_3C;
        const GREEN: u32 = 0x00_3E_CF_8E;
        if self.volume.is_none() && self.no_audio_at.is_some_and(|t| t.elapsed() < Duration::from_secs(4)) {
            return Some((self.no_audio_note.into(), YELLOW, 1.0));
        }
        if let Some(t) = *self.input.revoked_at.lock().unwrap() {
            if t.elapsed() < Duration::from_secs(5) {
                return Some(("Показывающий отключил управление".into(), RED, 1.0));
            }
        }
        if !self.ctl_available() || !self.input.want.load(Ordering::Relaxed) {
            return None;
        }
        let info = self.input.host_info()?;
        if !info.allowed {
            return Some(("Ждём, пока показывающий разрешит управление".into(), YELLOW, 1.0));
        }
        if self.captured {
            let fresh = self.capture_t.is_some_and(|t| t.elapsed() < Duration::from_secs(6));
            return Some(if fresh {
                ("Управление включено · Ctrl+Alt+Shift (или держите Esc 1 с) — отпустить мышь".into(), GREEN, 1.0)
            } else {
                ("Управление включено".into(), GREEN, 0.6)
            });
        }
        if info.kbm {
            let t = if info.pad && self.input.pad_count() > 0 {
                "Геймпад уже передаётся. Нажмите на картинку, чтобы управлять мышью и клавиатурой"
            } else if !info.pad && self.input.pad_count() > 0 {
                "Нажмите на картинку, чтобы управлять мышью и клавиатурой (геймпад не разрешён)"
            } else {
                "Нажмите на картинку, чтобы управлять мышью и клавиатурой"
            };
            return Some((t.into(), GREEN, 1.0));
        }
        if info.pad {
            return Some(("Геймпад передаётся показывающему (мышь и клавиатура не разрешены)".into(), GREEN, 1.0));
        }
        Some(("Показывающий не разрешил ни геймпад, ни мышь с клавиатурой".into(), YELLOW, 1.0))
    }

    fn overlay_active(&self) -> bool {
        self.pad_panel || self.captured || self.hint().is_some()
    }

    fn save_volume(&self) {
        if let Some(v) = &self.volume {
            volume::save(v);
        }
    }

    fn toggle_fullscreen(&mut self) {
        let Some(w) = &self.window else { return };
        let fs = w.fullscreen().is_none();
        w.set_fullscreen(fs.then_some(Fullscreen::Borderless(None)));
        // In full screen the stream already shows the host's cursor, ours only appears together with the bar.
        self.wake_bar();
    }

    /// What the pointer is over; only counts while the bar is actually visible.
    fn set_hover(&mut self) {
        let visible = self.drag
            || self.hover != Hit::Outside
            || crate::guilogic::bar_opacity(self.last_move.elapsed().as_millis()) > 0.05;
        self.hover = match self.bar_geometry() {
            Some(b) if visible => b.hit(self.cursor.0, self.cursor.1),
            _ => Hit::Outside,
        };
    }
}

impl ApplicationHandler<UserEv> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes()
            .with_title("screenshare — F11 или двойной клик: полный экран / окно")
            .with_window_icon(crate::icon::window_icon())
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        if self.start_fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        self.cursor_shown = !self.start_fullscreen;
        window.set_cursor_visible(self.cursor_shown);
        self.last_move = Instant::now();
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
            WindowEvent::Focused(false) => {
                // Leaving the window lets go of the mouse and of every key.
                self.set_capture(false);
                self.input.release_kbm();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                if self.captured && self.mods.control_key() && self.mods.alt_key() && self.mods.shift_key() {
                    self.set_capture(false);
                }
            }
            // While the mouse is captured everything goes to the host (the release chord is Ctrl+Alt+Shift).
            WindowEvent::KeyboardInput { event, .. } if self.captured => self.forward_key(&event),
            WindowEvent::MouseInput { state, button, .. } if self.captured => {
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 3,
                    MouseButton::Forward => 4,
                    MouseButton::Other(_) => return,
                };
                self.input.mouse_button(b, state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } if self.captured => {
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 120.0, y * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 * 3.0, p.y as f32 * 3.0),
                };
                self.input.wheel(y.round() as i32, x.round() as i32);
            }
            WindowEvent::CursorMoved { position, .. } if self.captured => {
                if self.laser {
                    self.laser = false;
                    self.input.laser(None);
                }
                if !self.relative_mode {
                    let spot = self.picture_spot(position);
                    self.input.mouse_abs(spot);
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat && matches!(event.logical_key, Key::Named(NamedKey::F2)) =>
            {
                // Gamepad test panel.
                if self.control {
                    self.pad_panel = !self.pad_panel;
                    self.redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat && matches!(event.logical_key, Key::Named(NamedKey::F3)) =>
            {
                // «Кино» <-> «Игра» while watching (remembered for next time).
                let game = !self.sh.game.load(Ordering::Relaxed);
                self.sh.game.store(game, Ordering::Relaxed);
                if let Some(q) = &self.audio_q {
                    q.set_game(game);
                }
                crate::volume::write_setting("viewer_mode", if game { "game" } else { "movie" });
                if let Some(w) = &self.window {
                    w.set_title(if game {
                        "screenshare — режим «Игра»: минимальная задержка (F3 — вернуть «Кино»)"
                    } else {
                        "screenshare — режим «Кино»: звук и картинка вместе (F3 — «Игра»)"
                    });
                }
            }
            WindowEvent::Resized(s) => {
                self.sh.win_size.store(((s.width as u64) << 32) | s.height as u64, Ordering::Relaxed);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat && self.annot_key(&event) => {}
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed && !event.repeat => {
                match event.logical_key {
                    Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
                    Key::Named(NamedKey::Escape) => {
                        if self.window.as_ref().is_some_and(|w| w.fullscreen().is_some()) {
                            self.toggle_fullscreen();
                        }
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("m") => {
                        if let Some(v) = &self.volume {
                            v.toggle_mute();
                            volume::save(v);
                        }
                        self.wake_bar();
                    }
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let moved = (position.x as i32 - self.cursor.0).abs() + (position.y as i32 - self.cursor.1).abs() >= 2;
                self.cursor = (position.x as i32, position.y as i32);
                if self.laser {
                    self.input.laser(self.picture_spot(position));
                }
                if moved {
                    self.last_move = Instant::now();
                    self.set_cursor(true);
                }
                let before = self.hover;
                self.set_hover();
                if self.drag {
                    if let (Some(b), Some(v)) = (self.bar_geometry(), &self.volume) {
                        v.set_level(b.level_at(self.cursor.0));
                        v.set_muted(false);
                    }
                }
                if moved || before != self.hover {
                    self.redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.input.laser(None);
                self.hover = Hit::Outside;
                self.redraw();
            }
            // Right click: show the bar (and keep it up a little longer).
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Right, .. } => {
                self.wake_bar();
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                if self.drag {
                    self.drag = false;
                    self.save_volume();
                    self.last_move = Instant::now();
                }
            }
            // Wheel: volume in 5% steps, anywhere in the window.
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 40.0) as f32,
                };
                if let Some(v) = &self.volume {
                    v.set_level(volume::clamp_level(((v.level() + dy * 0.05) * 20.0).round() / 20.0));
                    v.set_muted(false);
                    volume::save(v);
                } else {
                    self.no_audio_at = Some(Instant::now());
                }
                self.wake_bar();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                // A click on the bar belongs to the bar (and does not count towards the double click).
                self.set_hover();
                match self.hover {
                    Hit::Outside if self.ctl_available() && self.input.kbm_active() => {
                        // A click on the picture takes the mouse for the host.
                        self.set_capture(true);
                    }
                    Hit::Outside => {
                        let now = Instant::now();
                        if self.last_click.is_some_and(|t| now.duration_since(t) < Duration::from_millis(400)) {
                            self.toggle_fullscreen();
                            self.last_click = None;
                        } else {
                            self.last_click = Some(now);
                        }
                    }
                    hit => {
                        self.last_click = None;
                        match hit {
                            Hit::Mute => match &self.volume {
                                Some(v) => {
                                    v.toggle_mute();
                                    volume::save(v);
                                }
                                None => self.no_audio_at = Some(Instant::now()),
                            },
                            Hit::Slider(level) => match &self.volume {
                                Some(v) => {
                                    v.set_level(level);
                                    v.set_muted(false);
                                    self.drag = true;
                                }
                                None => self.no_audio_at = Some(Instant::now()),
                            },
                            Hit::Fullscreen => self.toggle_fullscreen(),
                            Hit::Control => self.toggle_control(),
                            Hit::Stop => {
                                self.save_volume();
                                el.exit();
                            }
                            _ => {}
                        }
                        self.wake_bar();
                    }
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn new_events(&mut self, _el: &ActiveEventLoop, cause: StartCause) {
        // The bar animates (fade, lamp pulse) only while it is visible; this timer is the only thing that wakes us.
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            self.redraw();
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, ev: DeviceEvent) {
        // Raw mouse movement (no pointer acceleration, not stopped by the window edge) while captured.
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = ev {
            if self.captured {
                let x = dx + self.mouse_frac.0;
                let y = dy + self.mouse_frac.1;
                let (ix, iy) = (x.trunc(), y.trunc());
                self.mouse_frac = (x - ix, y - iy);
                self.input.mouse_move(ix as i32, iy as i32);
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // The host switched control off (or it was revoked): the mouse is let go at once.
        if self.captured && !self.input.kbm_active() {
            self.set_capture(false);
        }
        if self.captured && self.esc_since.is_some_and(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.set_capture(false);
        }
        self.apply_pointer_mode();
        // The overlay (hint, F2 panel, captured mouse) and the bar animate on the clock only: ResumeTimeReached in
        // `new_events` redraws, so this must not ask for a redraw itself (that made a loop that never slept).
        if crate::guilogic::viewer_ticks(self.overlay_active(), self.bar_opacity() > 0.0, self.bar_drawn) {
            el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + BAR_TICK));
        } else {
            el.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl App {
    fn draw(&mut self) {
        let t0 = Instant::now();
        let ctl = if self.ctl_available() {
            let hi = self.input.host_info();
            Some((
                self.input.want.load(Ordering::Relaxed),
                hi.is_some_and(|i| i.allowed),
                self.input.pad_count(),
                hi.is_some_and(|i| i.pad),
                hi.is_some_and(|i| i.kbm),
            ))
        } else {
            None
        };
        let hint = self.hint();
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
        let new_picture = p.seq != self.shown_seq;
        self.shown_seq = p.seq;
        let (pw, ph) = (p.w as usize, p.h as usize);
        if pw == 0 || p.px.len() < pw * ph {
            buf.fill(0);
        } else if !crate::scale::matches_window(pw, ph, ww, wh) {
            // The window was resized and the decoder has not caught up yet: stretch smoothly instead of black bars.
            crate::scale::blit_fit(&mut buf, ww, wh, &p.px, pw, ph);
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
        let opacity = if self.drag || self.hover != Hit::Outside {
            1.0
        } else {
            crate::guilogic::bar_opacity(self.last_move.elapsed().as_millis())
        };
        self.bar_drawn = opacity > 0.0;
        let sc = (w.scale_factor().round() as i32).max(1);
        if opacity > 0.0 {
            let st = bar::State {
                level: self.volume.as_ref().map_or(1.0, |v| v.level()),
                muted: self.volume.as_ref().is_some_and(|v| v.muted()),
                has_audio: self.volume.is_some(),
                net: Some(self.sh.net_level.load(Ordering::Relaxed)).filter(|&n| n < 3),
                fullscreen: w.fullscreen().is_some(),
                hover: self.hover,
                opacity,
                time: self.started.elapsed().as_secs_f32(),
                ctl: ctl.map(|(on, allowed, pads, pad_ok, kbm_ok)| bar::Ctl { on, allowed, pads: pads as u8, captured: self.captured, pad_ok, kbm_ok }),
            };
            let geom = Bar::with_control(ww as i32, wh as i32, sc, ctl.is_some());
            bar::draw(&mut buf, ww as i32, wh as i32, &geom, &st);
            if geom.ctl_w > 90 * sc {
                if let Some((on, allowed, _, pad_ok, kbm_ok)) = ctl {
                    let mut ob = crate::overlay::Buf { px: &mut buf, w: ww as i32, h: wh as i32 };
                    let col = if on && allowed { 0x00_FF_FF_FF } else if on { 0x00_F0_B4_3C } else { 0x00_E8_EA_EE };
                    // faded with the bar through the label's colour: simple, the label is small
                    if opacity > 0.5 {
                        let label = bar::Ctl { on, allowed, pads: 0, captured: false, pad_ok, kbm_ok }.label();
                        crate::overlay::bar_label(&mut ob, geom.ctl_rect(), sc, label, col);
                    }
                }
            }
        }
        // Remote-control overlays: the hint line and the F2 gamepad test.
        {
            let mut ob = crate::overlay::Buf { px: &mut buf, w: ww as i32, h: wh as i32 };
            if self.laser && !self.captured {
                // Our own dot right away (the host's comes back only with the picture).
                ob.disc(self.cursor.0, self.cursor.1, 9.0 * sc as f32, 0x00_FF_30_30, 0.3);
                ob.disc(self.cursor.0, self.cursor.1, 4.5 * sc as f32, 0x00_FF_30_30, 1.0);
            }
            if let Some((text, dot, a)) = hint.as_ref() {
                crate::overlay::hint(&mut ob, ww as i32 / 2, 20 * sc, sc, text, Some(*dot), *a);
            }
            if self.pad_panel {
                let slots = *self.input.slots_view();
                let pads = *self.input.pads.lock().unwrap();
                let mut rows = Vec::new();
                for (slot, st) in pads.iter().enumerate() {
                    if let Some(st) = st {
                        let virt = slots.iter().position(|s| *s == Some(slot)).map(|v| v + 1);
                        rows.push(crate::overlay::PadRow { virt, slot, st: *st });
                    }
                }
                let note = match self.input.host_info() {
                    Some(i) if i.allowed && i.pad && self.input.want.load(Ordering::Relaxed) => "Управление включено: геймпад передаётся показывающему.",
                    Some(i) if i.allowed && !i.pad => "Показывающий не разрешил геймпад.",
                    Some(i) if !i.allowed => "Показывающий пока не разрешил управление.",
                    Some(_) => "Включите «Управление» в панели внизу, чтобы геймпад попал в игру.",
                    None => "Показывающий не поддерживает удалённое управление (нужна версия 0.11.0).",
                };
                crate::overlay::pad_panel(&mut ob, sc, &rows, note, 1.0);
            }
        }
        // The mouse cursor goes away with the bar in full screen (the stream already shows the host's).
        let hide = opacity <= 0.0 && w.fullscreen().is_some();
        // While controlling, the pointer mode decides (it used to come back here and drift away from the host's).
        let want_cursor = if self.captured { !self.relative_mode } else { !hide };
        let _ = buf.present();
        if new_picture {
            self.sh.diag.lock().unwrap().shown.feed(self.sh.start.elapsed().as_micros() as u64);
        }
        if self.cursor_shown != want_cursor {
            self.cursor_shown = want_cursor;
            w.set_cursor_visible(want_cursor);
        }
        self.sh.diag.lock().unwrap().present.add(t0.elapsed().as_micros() as u64);
        if flash {
            crate::avsync::log("client_video", crate::avsync::now_us());
        }
    }
}

/// Hidden `overlaytest`: draws the bar, the hint line and the F2 panel over a fake frame into a BMP (for screenshots without a connection).
pub fn overlay_preview(out: &std::path::Path, panel: bool, on: bool) -> std::io::Result<()> {
    let ww = std::env::var("OVERLAY_W").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1280).clamp(320, 3840);
    let wh = 720usize;
    // OVERLAY_DEV=pad|kbm|none: what the host allows (default both).
    let dev = std::env::var("OVERLAY_DEV").unwrap_or_default();
    let (pad_ok, kbm_ok) = (dev != "kbm" && dev != "none", dev != "pad" && dev != "none");
    let mut buf = vec![0u32; ww * wh];
    for y in 0..wh {
        for x in 0..ww {
            let (r, g, b) = (40 + x * 60 / ww, 60 + y * 80 / wh, 90 + (x + y) * 50 / (ww + wh));
            buf[y * ww + x] = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
        }
    }
    let st = bar::State {
        level: 0.7,
        muted: false,
        has_audio: true,
        net: Some(0),
        fullscreen: false,
        hover: Hit::Outside,
        opacity: 1.0,
        time: 1.0,
        ctl: Some(bar::Ctl { on, allowed: true, pads: 2, captured: on && kbm_ok, pad_ok, kbm_ok }),
    };
    let geom = Bar::with_control(ww as i32, wh as i32, 1, true);
    bar::draw(&mut buf, ww as i32, wh as i32, &geom, &st);
    let mut ob = crate::overlay::Buf { px: &mut buf, w: ww as i32, h: wh as i32 };
    let label = match st.ctl {
        Some(c) => c.label(),
        None => "Управление",
    };
    if geom.ctl_w > 90 {
        crate::overlay::bar_label(&mut ob, geom.ctl_rect(), 1, label, if on { 0x00_FF_FF_FF } else { 0x00_E8_EA_EE });
    }
    if on {
        crate::overlay::hint(&mut ob, ww as i32 / 2, 20, 1, "Управление: мышь захвачена. Выйти — Ctrl+Alt+Shift или удерживайте Esc", Some(0x00_3C_D2_78), 1.0);
    }
    if panel {
        let mut rows = Vec::new();
        if let Some(p) = crate::xinput::XInput::load() {
            for slot in 0..4 {
                if let Some(st) = p.state(slot) {
                    rows.push(crate::overlay::PadRow { virt: Some(rows.len() + 1), slot, st });
                }
            }
        }
        crate::overlay::pad_panel(&mut ob, 1, &rows, "Управление включено: геймпад передаётся показывающему.", 1.0);
    }
    write_bmp(out, ww as u32, wh as u32, &buf)
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
