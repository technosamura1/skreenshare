// Host side: capture -> NVENC -> paced UDP. Audio goes straight from the WASAPI callback.
use crate::capture::{self, Capturer, Event, MonitorInfo};
use crate::audiopkt::AudioTx;
use crate::net::{self, Code};
use crate::audiosel::{self, AudioPlan, AudioPrefs};
use crate::netq::{NetQuality, Sample, Side};
use crate::ratectl::{RateControl, ReportTracker};
use crate::retx::{self, pacer_rate, KeyGate, Poll};
use crate::ui::{self, Exchange};
use crate::video::{BgraScaler, Encoder, EncoderConfig};
use crate::wincap::WindowCapturer;
use crate::preview;
use anyhow::{bail, Result};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct HostOpts {
    pub monitor: Option<usize>,
    /// Stream only this window (HWND). The stream keeps the monitor's resolution; the window is letterboxed in it.
    pub window: Option<isize>,
    /// "Авто": start low and ramp up to `bitrate_mbps` (the ceiling) while the link is clean.
    pub auto_bitrate: bool,
    pub bitrate_mbps: u32,
    pub fps: u32,
    pub preset: String,
    pub max_send_mbps: u32,
    pub audio: bool,
    pub audio_prefs: AudioPrefs,
    pub exchange: Exchange,
    /// What the viewer may control at the start (the window can change it while streaming). Nothing by default.
    pub control: crate::inputlogic::Perms,
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

/// Frames waiting for the paced sender beyond which new screen updates are skipped (the channel holds 6).
const MAX_BACKLOG: usize = 3;

struct Shared {
    running: AtomicBool,
    keyreq: AtomicBool,
    start: Instant,
    last_seen_ms: AtomicU64,
    sent_bytes: AtomicU64,
    resent: AtomicU64,
    /// Raw packet loss in the last client reports, in 1/10000 (for the status line).
    loss_bp: AtomicU32,
    /// Pacer rate, bytes per second.
    send_rate: AtomicU64,
    /// The viewer announced (PT_CAPS) that it plays Opus audio.
    peer_opus: AtomicBool,
    /// Recent Opus frames, for answering the viewer's audio NACKs (set once the Opus sender exists).
    audio_hist: Mutex<Option<crate::opusaudio::History>>,
    /// Encoded video frames the paced sender has not finished sending yet (in the channel or being sent).
    backlog: AtomicUsize,
    /// Frames skipped because the sender was still busy (instead of encoding them and breaking the chain).
    skipped_busy: AtomicU64,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

enum AudioSource {
    Off,
    /// Process loopback: only one window's process, or everything except the voice app / ourselves.
    Process(#[allow(dead_code)] crate::loopback::Capture),
    /// Plain loopback of the whole system; only used when process loopback is unavailable and nothing must be excluded.
    Everything(#[allow(dead_code)] cpal::Stream),
}

type AudioSink = Box<dyn FnMut(&[i16], u32) + Send>;

fn audio_sender(sock: Arc<UdpSocket>, peer: SocketAddr, token: u32, sh: Arc<Shared>, seq: Arc<AtomicU32>) -> AudioSink {
    // Every datagram carries the previous one's samples too (see audiopkt.rs), so one lost packet is rebuilt.
    let mut tx = AudioTx::new(token, seq.clone());
    // Opus (v0.7.4+) for viewers that announced it; 48 kHz only (the usual rate of the process loopback capture),
    // anything else keeps the older mu-law format that every viewer understands.
    let mut opus_tx: Option<crate::opusaudio::Tx> = None;
    let mut opus_failed = false;
    let mut beep = crate::avsync::BeepEdge::new(48_000);
    Box::new(move |samples, rate| {
        if crate::avsync::enabled() {
            let frames = samples.len() / 2;
            let now = crate::avsync::now_us();
            for (i, f) in samples.chunks_exact(2).enumerate() {
                if beep.feed(f[0] as f32 / 32768.0) {
                    crate::avsync::log("host_audio", now - ((frames - i) as i64 * 1_000_000 / rate as i64));
                }
            }
        }
        let mut send = |pkt: &[u8]| {
            net::send_media(&sock, pkt, peer);
            sh.sent_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        };
        if rate == 48_000 && sh.peer_opus.load(Ordering::Relaxed) && !opus_failed {
            if opus_tx.is_none() {
                match crate::opusaudio::Tx::new(token, seq.clone(), 128) {
                    Ok(t) => {
                        *sh.audio_hist.lock().unwrap() = Some(t.history.clone());
                        opus_tx = Some(t);
                    }
                    Err(e) => {
                        eprintln!("Opus недоступен ({e:#}), звук идёт в старом формате");
                        opus_failed = true;
                    }
                }
            }
            if let Some(t) = opus_tx.as_mut() {
                t.push(samples, &mut send);
                return;
            }
        }
        tx.push(samples, rate, &mut send);
    })
}

/// What to capture for the current source, given the user's checkboxes: see `audiosel::choose`.
fn plan_audio(prefs: AudioPrefs, window: Option<isize>) -> AudioPlan {
    let window_pid = window.and_then(crate::wincap::audio_pid_of);
    audiosel::choose(prefs, window_pid, crate::loopback::find_voice_app().map(|v| v.0), std::process::id())
}

/// Starts the capture for `want`. Never streams the voice chat back to the friend: if a running voice app should be
/// excluded and can't be, no audio at all. `fallback` is what is used when "only this window" is not possible
/// (older Windows): the same choice with the window checkbox off. `shown` is the window title, for the status line.
fn start_audio(make_sink: &dyn Fn() -> AudioSink, want: AudioPlan, fallback: AudioPlan, shown: Option<&str>) -> AudioSource {
    use crate::loopback::{self, Mode};
    // Sync test: capture only the beacon, so the viewer's own playback on this PC isn't re-captured.
    if let Some(pid) = std::env::var("SS_AUDIO_ONLY_PID").ok().and_then(|v| v.parse().ok()) {
        return match loopback::start(pid, Mode::Include, make_sink()) {
            Ok(c) => AudioSource::Process(c),
            Err(e) => {
                ui::audio_status(&format!("Звук не захватывается: {e:#}"));
                AudioSource::Off
            }
        };
    }
    let mut window_note = String::new();
    if let AudioPlan::OnlyProcess(pid) = want {
        match loopback::start(pid, Mode::Include, make_sink()) {
            Ok(c) => {
                ui::audio_status(&format!(
                    "Звук: только из окна «{}» — другие программы и Discord не слышны",
                    crate::source::ellipsize(shown.unwrap_or("окно"), 40)
                ));
                return AudioSource::Process(c);
            }
            Err(e) => window_note = format!(" (звук только из окна здесь недоступен: {e:#})"),
        }
    }
    let target = if matches!(want, AudioPlan::OnlyProcess(_)) { fallback.pid() } else { want.pid() };
    let me = std::process::id();
    let voice = loopback::find_voice_app().filter(|v| v.0 == target);
    match loopback::start(target, Mode::Exclude, make_sink()) {
        Ok(c) => {
            ui::audio_status(&match (&voice, target == me) {
                (Some((_, name)), _) => format!("Звук: передаётся всё, кроме {name} — эха не будет{window_note}"),
                (None, true) if loopback::find_voice_app().is_some() => {
                    format!("Звук: передаётся весь звук компьютера, Discord тоже слышен{window_note}")
                }
                _ => format!("Звук: передаётся весь звук компьютера{window_note}"),
            });
            AudioSource::Process(c)
        }
        Err(e) if voice.is_some() => {
            ui::audio_status(&format!(
                "Звук выключен: эта Windows не умеет исключать {} из захвата, а иначе зритель слышал бы себя ({e:#})",
                voice.unwrap().1
            ));
            AudioSource::Off
        }
        Err(_) => match crate::audio::start_capture(make_sink()) {
            Ok(s) => {
                ui::audio_status(&format!("Звук: передаётся весь звук компьютера{window_note}"));
                AudioSource::Everything(s)
            }
            Err(e) => {
                ui::audio_status(&format!("Звук не захватывается: {e:#}"));
                AudioSource::Off
            }
        },
    }
}

/// Where the picture comes from: the whole monitor or one window.
enum Cap {
    Screen(Capturer),
    Window(Box<WindowCapturer>),
}

/// Opens window capture, or says why it can't be done (the caller then shows the full screen).
fn open_window(h: isize) -> Result<Cap> {
    Ok(Cap::Window(Box::new(WindowCapturer::new(h)?)))
}

/// Switch to the full screen after the window was lost: a visible message and a fresh keyframe.
fn fall_back_to_screen(why: &str, monitor: &MonitorInfo) -> Result<Cap> {
    ui::source_status(&format!("{why} — показываю весь экран"), true);
    ui::source_label(&crate::source::source_label(None));
    Ok(Cap::Screen(Capturer::new(monitor)?))
}

/// What a live source switch produced.
struct Switched {
    cap: Cap,
    /// Set when another monitor was chosen (its size is the new canvas).
    monitor: Option<MonitorInfo>,
    /// The window title when a window is shown now.
    title: Option<String>,
    status: String,
    label: String,
}

/// Opens the requested source *before* the running one is touched: on an error the caller just keeps streaming
/// what it had. `current` is the monitor being streamed (the canvas of every window).
fn open_requested(req: crate::source::SourceReq, current: &MonitorInfo, showing_screen: bool) -> Result<Option<Switched>> {
    use crate::source::SourceReq;
    match req {
        SourceReq::Window(h) => {
            let title = crate::wincap::title_of(h);
            let c = open_window(h)?;
            let note = match &c {
                Cap::Window(w) if w.border == crate::wincap::BorderState::Unavailable => crate::source::BORDER_NOTE,
                _ => "",
            };
            Ok(Some(Switched {
                status: format!(
                    "Показываю окно «{}» (в кадре {}×{}){note}",
                    crate::source::ellipsize(&title, 50),
                    current.width & !1,
                    current.height & !1
                ),
                label: crate::source::source_label(Some(&title)),
                cap: c,
                monitor: None,
                title: Some(title),
            }))
        }
        SourceReq::Monitor(i) => {
            let mut mons = capture::list_monitors()?;
            if i >= mons.len() {
                bail!("монитора {} нет (есть {})", i + 1, mons.len());
            }
            let multi = mons.len() > 1;
            let m = mons.swap_remove(i);
            if showing_screen && m.same_output(current) {
                return Ok(None); // already showing exactly this
            }
            let c = Cap::Screen(Capturer::new(&m)?);
            let (status, label) = if m.primary || !multi {
                ("Показываю весь экран".to_string(), crate::source::source_label(None))
            } else {
                (format!("Показываю монитор {} ({}×{})", i + 1, m.width, m.height), format!("Монитор {}", i + 1))
            };
            Ok(Some(Switched { cap: c, monitor: Some(m), title: None, status, label }))
        }
    }
}

/// The micro self-preview for the GUI. Works from the frame the encoder was just given, so it shows exactly
/// what is sent. Cheap by design: nothing at all unless the GUI is looking (heartbeat), at most ~6.7 pictures a
/// second, area-downscale of the frame (~ms), RGB packing, and the heavy part (compression, base64, the pipe
/// write) on its own thread, with a one-slot queue that drops the picture when that thread is busy.
struct Preview {
    gate: preview::Gate,
    scaler: BgraScaler,
    small: Vec<u8>,
    rgb: Vec<u8>,
    tx: mpsc::SyncSender<(u32, u32, Vec<u8>)>,
    /// A frame was encoded since the last picture (a static screen is not re-sent).
    stale: bool,
    /// The encoder holds a real frame (not the blank buffer of a just-created encoder).
    has_frame: bool,
    was_on: bool,
    /// When the last picture was made (host clock, ms).
    last_ms: u64,
}

impl Preview {
    fn new() -> (Preview, std::thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::sync_channel::<(u32, u32, Vec<u8>)>(1);
        let th = std::thread::spawn(move || {
            for (w, h, rgb) in rx {
                if let Ok(packed) = zstd::bulk::compress(&rgb, 1) {
                    ui::preview_line(&preview::encode_line(w, h, &packed));
                }
            }
        });
        let p = Preview {
            gate: preview::Gate::new(),
            scaler: BgraScaler::for_preview(),
            small: Vec::new(),
            rgb: Vec::new(),
            tx,
            stale: false,
            has_frame: false,
            was_on: false,
            last_ms: 0,
        };
        (p, th)
    }

    /// The encoder was (re)created: its buffer holds nothing to show yet.
    fn reset(&mut self) {
        self.has_frame = false;
        self.stale = false;
    }

    /// A frame has just been encoded.
    fn note_frame(&mut self) {
        self.has_frame = true;
        self.stale = true;
    }

    /// `starved_after_ms`: 0 = called in an idle moment; otherwise called right after an encode and only works
    /// if no picture was made for that long. A busy stream (encoder running behind the capture rate, e.g. a
    /// game at 50 fps) has no idle moments at all, and the preview used to starve there.
    fn maybe(&mut self, enc: Option<&Encoder>, canvas: (u32, u32), now_ms: u64, starved_after_ms: u64) {
        if starved_after_ms > 0 && now_ms < self.last_ms + starved_after_ms {
            return;
        }
        let on = ui::preview_wanted();
        if on && !self.was_on {
            self.stale = self.has_frame; // window shown again: one fresh picture right away
        }
        self.was_on = on;
        if !on || !self.stale || !self.has_frame || !self.gate.due(now_ms) {
            return;
        }
        let Some(e) = enc else { return };
        let t0 = Instant::now();
        let (src, stride) = e.last_input();
        let (pw, ph) = preview::preview_size(canvas.0, canvas.1);
        self.small.resize(pw as usize * ph as usize * 4, 0);
        if self.scaler.scale(src, canvas.0, canvas.1, stride, &mut self.small, pw, ph, pw as usize * 4).is_err() {
            return;
        }
        preview::pack_rgb(&self.small, pw, ph, pw as usize * 4, &mut self.rgb);
        let _ = self.tx.try_send((pw, ph, std::mem::take(&mut self.rgb))); // busy thread: skip this one
        self.stale = false;
        self.last_ms = now_ms;
        self.gate.done(now_ms, t0.elapsed().as_micros() as u64);
    }
}

pub fn run(o: HostOpts) -> Result<()> {
    let interactive = matches!(o.exchange, Exchange::Console) && !ui::ipc();
    let monitor = pick_monitor(o.monitor, interactive)?;
    println!("Показываю монитор {}x{} ({})", monitor.width, monitor.height, monitor.adapter);

    println!("Определяю адрес в интернете...");
    let (sock, nat) = net::open_and_discover()?;
    let sock = Arc::new(sock);
    {
        let (r, s) = net::socket_buffers(&sock);
        println!("Буферы сокета: приём {} КБ, отправка {} КБ", r / 1024, s / 1024);
    }
    ui::report_nat(&nat);
    let token = net::random_token();
    let me = Code::for_socket(net::ROLE_HOST, token, &sock, &nat);
    ui::show_code(&o.exchange, &me.encode(), "host.code", "Ваш код — передайте его зрителю:")?;
    let peer_code = ui::read_code(
        &o.exchange,
        net::ROLE_CLIENT,
        Some(token),
        "client.code",
        "Вставьте сюда код ответа от зрителя и нажмите Enter:",
        &sock,
        nat.keepalive_server,
    )?;
    println!("Соединяюсь...");
    let peer = net::punch(&sock, net::ROLE_HOST, token, &peer_code.cands, Duration::from_secs(30))?;
    println!("Подключено! Идёт трансляция.");
    ui::connected();
    stream(sock, peer, token, monitor, &o)
}

fn stream(sock: Arc<UdpSocket>, peer: SocketAddr, token: u32, mut monitor: MonitorInfo, o: &HostOpts) -> Result<()> {
    let sh = Arc::new(Shared {
        running: AtomicBool::new(true),
        keyreq: AtomicBool::new(true),
        start: Instant::now(),
        last_seen_ms: AtomicU64::new(0),
        sent_bytes: AtomicU64::new(0),
        resent: AtomicU64::new(0),
        loss_bp: AtomicU32::new(0),
        send_rate: AtomicU64::new(pacer_rate(if o.auto_bitrate { crate::ratectl::auto_start(o.bitrate_mbps as f64) } else { o.bitrate_mbps as f64 }, o.max_send_mbps)),
        peer_opus: AtomicBool::new(false),
        audio_hist: Mutex::new(None),
        backlog: AtomicUsize::new(0),
        skipped_busy: AtomicU64::new(0),
    });
    {
        let sh = sh.clone();
        ui::on_stop(move || sh.running.store(false, Ordering::Relaxed));
    }
    // Remote control: its own thread (timeout, emergency hotkey, answers); packets are applied by the receiver below.
    let input = crate::inputhost::InputHost::new(o.control);
    {
        let sh = sh.clone();
        input.spawn(sock.clone(), peer, token, move || sh.running.load(Ordering::Relaxed));
    }
    let (nack_tx, nack_rx) = mpsc::channel::<(u32, Vec<u16>)>();
    let (rep_tx, rep_rx) = mpsc::channel::<crate::ratectl::ReportDelta>();

    // Control channel from the client.
    {
        let sock = sock.clone();
        let sh = sh.clone();
        let input = input.clone();
        std::thread::spawn(move || {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = [0u8; 2048];
            let mut out = Vec::with_capacity(64);
            let mut tracker = ReportTracker::default();
            while sh.running.load(Ordering::Relaxed) {
                if sh.now_ms().saturating_sub(sh.last_seen_ms.load(Ordering::Relaxed)) > 10_000 {
                    println!("Соединение потеряно.");
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
                    net::PT_CAPS => {
                        sh.peer_opus.store(body.first().is_some_and(|f| f & net::CAP_OPUS != 0), Ordering::Relaxed);
                        input.set_peer_caps(body.first().copied().unwrap_or(0));
                    }
                    // Remote control: only from the connected viewer's address, and only acted on if the owner allowed it.
                    net::PT_INPUT if from == peer => input.on_input(body),
                    net::PT_ANACK => {
                        // Resend audio the viewer is still missing (a few per request: it only asks for what it needs).
                        let hist = sh.audio_hist.lock().unwrap().clone();
                        if let Some(h) = hist {
                            for seq in crate::opusaudio::parse_anack(body).into_iter().take(8) {
                                if h.datagram(token, seq, &mut out) {
                                    net::send_media(&sock, &out, from);
                                    sh.sent_bytes.fetch_add(out.len() as u64, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    net::PT_NACK => {
                        if let Some(n) = retx::parse_nack(body) {
                            let _ = nack_tx.send(n);
                        }
                    }
                    net::PT_REPORT => {
                        if let Some(d) = tracker.on_report(body) {
                            sh.loss_bp.store((d.missed * 10_000 / d.total().max(1)) as u32, Ordering::Relaxed);
                            let _ = rep_tx.send(d);
                        }
                    }
                    net::PT_BYE => {
                        println!("Зритель отключился.");
                        sh.running.store(false, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        });
    }

    // Paced sender. Keeps recent frames' datagrams so packets the client reports lost can be resent
    // (retransmissions go out before new frames, see retx.rs).
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, bool, u32)>(6);
    let sender = {
        let sock = sock.clone();
        let sh = sh.clone();
        std::thread::spawn(move || {
            let mut s = retx::Sender::new(token, Instant::now());
            let mut closed = false;
            loop {
                let now = Instant::now();
                while let Ok((fid, idxs)) = nack_rx.try_recv() {
                    s.on_nack(now, fid, &idxs);
                }
                if !s.busy() && !closed {
                    match rx.try_recv() {
                        Ok((data, key, fid)) => {
                            sh.backlog.fetch_sub(1, Ordering::Relaxed);
                            s.push_frame(fid, key, &data)
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                        Err(mpsc::TryRecvError::Disconnected) => closed = true,
                    }
                }
                match s.poll(now, sh.send_rate.load(Ordering::Relaxed) as f64) {
                    Poll::Send(p) => {
                        net::send_media(&sock, &p, peer);
                        sh.sent_bytes.fetch_add(p.len() as u64, Ordering::Relaxed);
                        sh.resent.store(s.resent, Ordering::Relaxed);
                    }
                    Poll::Wait(d) if d > Duration::from_millis(2) => std::thread::sleep(Duration::from_millis(1)),
                    Poll::Wait(_) => std::thread::yield_now(),
                    Poll::Idle if closed => break,
                    Poll::Idle => match rx.recv_timeout(Duration::from_millis(2)) {
                        Ok((data, key, fid)) => {
                            sh.backlog.fetch_sub(1, Ordering::Relaxed);
                            s.push_frame(fid, key, &data)
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => closed = true,
                    },
                }
            }
        })
    };

    let seq = Arc::new(AtomicU32::new(0));
    let sender_for_audio = || audio_sender(sock.clone(), peer, token, sh.clone(), seq.clone());
    let mut prefs = o.audio_prefs;

    // The stream canvas is the monitor's size whatever is shown, so going from a window back to the full screen
    // (or the other way round) never changes the resolution the viewer gets.
    let mut canvas = (monitor.width & !1, monitor.height & !1);
    let mut window_title = None;
    let mut cap = match o.window {
        Some(h) => {
            let title = crate::wincap::title_of(h);
            match open_window(h) {
                Ok(c) => {
                    let note = match &c {
                        Cap::Window(w) if w.border == crate::wincap::BorderState::Unavailable => crate::source::BORDER_NOTE,
                        _ => "",
                    };
                    ui::source_status(
                        &format!(
                            "Показываю окно «{}» (в кадре {}×{}){note}",
                            crate::source::ellipsize(&title, 50),
                            canvas.0,
                            canvas.1
                        ),
                        false,
                    );
                    ui::source_label(&crate::source::source_label(Some(&title)));
                    window_title = Some(title);
                    c
                }
                Err(e) => fall_back_to_screen(&format!("Окно «{}» не удалось захватить ({e:#})", crate::source::ellipsize(&title, 40)), &monitor)?,
            }
        }
        None => {
            ui::source_status("Показываю весь экран", false);
            ui::source_label(&crate::source::source_label(None));
            Cap::Screen(Capturer::new(&monitor)?)
        }
    };
    // The window whose process is the sound source (None while a whole screen is shown).
    let mut audio_window = match &cap {
        Cap::Window(_) => o.window,
        Cap::Screen(_) => None,
    };
    let mut audio_want = plan_audio(prefs, audio_window);
    let mut audio = if o.audio {
        start_audio(&sender_for_audio, audio_want, plan_audio(AudioPrefs { window_only: false, ..prefs }, None), window_title.as_deref())
    } else {
        AudioSource::Off
    };
    let mut audio_check = Instant::now();
    let mut audio_dirty = false;
    let (mut pv, pv_thread) = Preview::new();
    let mut scaler = BgraScaler::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut period = Duration::from_secs_f64(1.0 / o.fps as f64);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut enc_short = String::new();
    let mut enc: Option<Encoder> = None;
    let mut dirty = true;
    let mut force_key = true;
    let mut next_due = Instant::now();
    let mut gate = KeyGate::new(Instant::now());
    let mut fid = 0u32;
    let mut stat_t = Instant::now();
    let mut stat_frames = 0u32;
    let mut stat_enc = Duration::ZERO;
    let mut stat_bytes0 = 0u64;
    let mut flash = crate::avsync::FlashEdge::default();
    let mut rc = if o.auto_bitrate {
        RateControl::new_auto(o.bitrate_mbps as f64, Instant::now())
    } else {
        RateControl::new(o.bitrate_mbps as f64, Instant::now())
    };
    let rate_mode = |rc: &RateControl| if rc.ramp.is_some() { "ramp" } else if o.auto_bitrate { "auto" } else { "fixed" };
    if o.auto_bitrate {
        ui::rate(rc.cur, rate_mode(&rc));
    }
    // Network lamp: judged from each viewer report (loss, round-trip time, retransmissions, did we cut quality).
    let mut nq = NetQuality::new(Side::Host);
    let mut nq_t = Instant::now();
    let mut nq_emit = Instant::now() - Duration::from_secs(10);
    let (mut nq_bytes, mut nq_resent) = (0u64, 0u64);
    ui::net(&nq);
    // One graph point per second for the launcher (what was actually sent, ping, loss).
    let (mut gr_t, mut gr_bytes) = (Instant::now(), sh.sent_bytes.load(Ordering::Relaxed));
    let mut stat_resent0 = 0u64;
    let mut stat_updates = 0u32;
    let mut stat_skipped0 = 0u64;

    while sh.running.load(Ordering::Relaxed) {
        let loss = sh.loss_bp.load(Ordering::Relaxed) as f64 / 10_000.0;
        while let Ok(d) = rep_rx.try_recv() {
            let t = Instant::now();
            if let Some(mbps) = rc.update(t, d) {
                if let Some(e) = enc.as_mut() {
                    e.set_bitrate((mbps * 1e6) as u64);
                }
                sh.send_rate.store(pacer_rate(mbps, o.max_send_mbps), Ordering::Relaxed);
                if o.auto_bitrate {
                    ui::rate(mbps, rate_mode(&rc));
                }
            }
            let (bytes, resent) = (sh.sent_bytes.load(Ordering::Relaxed), sh.resent.load(Ordering::Relaxed));
            let pkts = (bytes.saturating_sub(nq_bytes) / 1200).max(1) as f64;
            let total = d.total().max(1) as f64;
            let sample = Sample {
                dt_ms: (t.duration_since(nq_t).as_millis() as u32).clamp(100, 2000),
                loss: d.missed as f64 / total,
                residual: d.residual as f64 / total,
                rtt_ms: (d.rtt_us > 0).then(|| d.rtt_us as f64 / 1000.0),
                retx: (resent.saturating_sub(nq_resent) as f64 / pkts).min(1.0),
                cut: rc.recently_cut(t),
                bitrate: rc.cur,
                max_bitrate: rc.max(),
            };
            (nq_t, nq_bytes, nq_resent) = (t, bytes, resent);
            if nq.update(sample).is_some() || nq_emit.elapsed() > Duration::from_secs(2) {
                nq_emit = Instant::now();
                ui::net(&nq);
            }
            if gr_t.elapsed() >= Duration::from_millis(950) {
                let secs = gr_t.elapsed().as_secs_f64();
                gr_t = t;
                let sent = sh.sent_bytes.load(Ordering::Relaxed);
                ui::graph(sent.saturating_sub(gr_bytes) as f64 * 8.0 / 1e6 / secs, sample.rtt_ms, sample.loss);
                gr_bytes = sent;
            }
        }
        // The sound checkboxes changed in the GUI.
        if let Some(l) = ui::take_audio_request() {
            if let Some(p) = prefs.parse_line(&l) {
                if p != prefs {
                    prefs = p;
                    audio_dirty = true;
                }
            }
        }
        // Re-aim the audio capture when something changed: the shown window / screen, the checkboxes, or Discord
        // started, quit or restarted (its voices must never leak in).
        if o.audio && (audio_dirty || audio_check.elapsed() > Duration::from_secs(3)) {
            audio_check = Instant::now();
            audio_dirty = false;
            let want = plan_audio(prefs, audio_window);
            if want != audio_want {
                audio_want = want;
                drop(std::mem::replace(&mut audio, AudioSource::Off)); // release the old capture first
                // The packet sequence is shared, so the viewer's timeline simply continues after the short gap.
                audio = start_audio(
                    &sender_for_audio,
                    want,
                    plan_audio(AudioPrefs { window_only: false, ..prefs }, None),
                    window_title.as_deref(),
                );
            }
        }
        let now = Instant::now();
        let timeout = if dirty { next_due.saturating_duration_since(now).as_millis() as u32 } else { 250 };
        let ev = match &mut cap {
            Cap::Screen(c) => c.next(timeout)?,
            Cap::Window(w) => match w.next(timeout) {
                Ok(ev) => ev,
                Err(e) => {
                    // Capture failure (device lost, ...): treated like a closed window.
                    println!("захват окна: {e:#}");
                    Event::Closed
                }
            },
        };
        match ev {
            Event::Updated => {
                dirty = true;
                stat_updates += 1;
            }
            Event::Reset => {
                dirty = true;
                if let Cap::Screen(c) = &cap {
                    canvas = (c.width, c.height);
                    if enc.as_ref().is_some_and(|e| e.width != c.width || e.height != c.height) {
                        enc = None;
                    }
                }
            }
            Event::Closed => {
                let title = window_title.take().unwrap_or_default();
                let why = if cap_window_gone(&cap) {
                    format!("Окно «{}» закрыто", crate::source::ellipsize(&title, 40))
                } else {
                    format!("Захват окна «{}» не удался", crate::source::ellipsize(&title, 40))
                };
                cap = fall_back_to_screen(&why, &monitor)?;
                audio_window = None; // back to the system sound without Discord
                audio_dirty = true;
                canvas = match &cap {
                    Cap::Screen(c) => (c.width, c.height),
                    Cap::Window(_) => canvas,
                };
                if enc.as_ref().is_some_and(|e| (e.width, e.height) != canvas) {
                    enc = None;
                }
                dirty = true;
                force_key = true;
                pv.reset(); // the next frame is the full screen
            }
            Event::Timeout => {}
        }
        // The GUI asked to show something else while streaming.
        if let Some(req) = ui::take_source_request() {
            let req_window = match &req {
                crate::source::SourceReq::Window(h) => Some(*h),
                _ => None,
            };
            match open_requested(req, &monitor, matches!(cap, Cap::Screen(_))) {
                Ok(Some(n)) => {
                    // The old capture is released only now, after the new one works.
                    cap = n.cap;
                    window_title = n.title;
                    audio_window = req_window.filter(|_| matches!(cap, Cap::Window(_)));
                    audio_dirty = true;
                    if let Some(m) = n.monitor {
                        monitor = m;
                    }
                    canvas = match &cap {
                        // A different-size monitor changes the stream size: new encoder, the viewer follows the new SPS.
                        Cap::Screen(c) => (c.width & !1, c.height & !1),
                        // A window is letterboxed into the canvas of the monitor, so the size stays.
                        Cap::Window(_) => (monitor.width & !1, monitor.height & !1),
                    };
                    if enc.as_ref().is_some_and(|e| (e.width, e.height) != canvas) {
                        enc = None;
                    }
                    ui::source_status(&n.status, false);
                    ui::source_label(&n.label);
                    dirty = true;
                    force_key = true; // the picture changes completely: a fresh reference for the viewer
                    pv.reset();
                }
                Ok(None) => ui::source_status("Этот экран уже показывается", false),
                Err(e) => ui::source_status(&format!("Не удалось переключить ({e:#}) — продолжаю показывать прежнее"), true),
            }
        }
        // Keyframe requests are a last resort on the client now; still served at most every 500 ms.
        if sh.keyreq.swap(false, Ordering::Relaxed) {
            gate.request();
        }
        if gate.take(Instant::now()) {
            force_key = true;
            dirty = true;
        }
        let now = Instant::now();
        if !dirty || now < next_due {
            // Idle moment: the right time for a preview of the last sent frame (never delays an encode).
            pv.maybe(enc.as_ref(), canvas, sh.now_ms(), 0);
            continue;
        }
        // The paced sender is still working through earlier frames (a big keyframe, retransmissions): don't pile more
        // on. Skipping *before* encoding keeps the reference chain intact; the old way (encode, then drop the
        // finished frame when the queue was full) forced a new keyframe, which is the biggest burst there is.
        if enc.is_some() && !force_key && sh.backlog.load(Ordering::Relaxed) >= MAX_BACKLOG {
            sh.skipped_busy.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        if enc.is_none() {
            let (e, note) = Encoder::open_best(
                &EncoderConfig {
                    width: canvas.0,
                    height: canvas.1,
                    fps: o.fps,
                    bitrate: (rc.cur * 1e6) as u64,
                    preset: o.preset.clone(),
                },
                cores,
            )?;
            ui::encoder(&e.label, note.as_deref());
            enc_short = match e.backend {
                crate::video::Backend::Nvenc => "NVENC".to_string(),
                crate::video::Backend::X264 => "x264 (процессор)".to_string(),
            };
            // Software encoding: its own frame rate and a bitrate the CPU can really produce.
            period = Duration::from_secs_f64(1.0 / e.fps as f64);
            if let Some(cap) = e.max_mbps {
                if cap < rc.max() {
                    rc.clamp_max(cap);
                    sh.send_rate.store(pacer_rate(rc.cur, o.max_send_mbps), Ordering::Relaxed);
                }
            }
            enc = Some(e);
            force_key = true;
            pv.reset();
        }
        let e = enc.as_mut().unwrap();
        let t0 = Instant::now();
        let (buf, stride) = e.input()?;
        match &mut cap {
            Cap::Screen(c) => c.compose(buf, stride),
            Cap::Window(w) => {
                let (src, size) = (w.pixels(), (w.width, w.height));
                crate::source::draw_letterboxed(src, size, buf, stride, canvas, &mut scratch, |s, sw, sh, d, dw, dh| {
                    scaler.scale(s, sw, sh, sw as usize * 4, d, dw, dh, dw as usize * 4).map_err(|e| format!("{e:#}"))
                })
                .map_err(|e| anyhow::anyhow!(e))?;
            }
        }
        if crate::avsync::enabled()
            && flash.rising(crate::avsync::luma(buf, stride, canvas.0 as usize, canvas.1 as usize))
        {
            crate::avsync::log("host_video", crate::avsync::now_us());
        }
        let key_now = force_key;
        e.encode(key_now, |data, key| {
            sh.backlog.fetch_add(1, Ordering::Relaxed);
            if tx.try_send((data.to_vec(), key, fid)).is_ok() {
                fid = fid.wrapping_add(1);
            } else {
                // Still full (it is deeper than MAX_BACKLOG, so this is rare): drop the frame, restart the chain.
                sh.backlog.fetch_sub(1, Ordering::Relaxed);
                sh.keyreq.store(true, Ordering::Relaxed);
            }
        })?;
        stat_enc += t0.elapsed();
        if key_now {
            gate.note_key(Instant::now());
        }
        force_key = false;
        dirty = false;
        pv.note_frame();
        pv.maybe(enc.as_ref(), canvas, sh.now_ms(), 400); // busy stream: no idle moments, so make one here
        next_due = if now.duration_since(next_due) > period { now + period } else { next_due + period };
        stat_frames += 1;

        if stat_t.elapsed() >= Duration::from_secs(5) {
            let secs = stat_t.elapsed().as_secs_f64();
            let bytes = sh.sent_bytes.load(Ordering::Relaxed);
            let resent = sh.resent.load(Ordering::Relaxed);
            let glitches = crate::audio::CAPTURE_GLITCHES.load(Ordering::Relaxed);
            let skipped = sh.skipped_busy.load(Ordering::Relaxed);
            ui::stat(&format!(
                "{enc_short} · {:.0} кадров/с (экран обновился {:.0}/с, отложено из-за сети {}) · {:.1} Мбит/с (качество {:.0}) · потери {:.1}% · дослано {} · кодирование {:.1} мс · звук {}{}",
                stat_frames as f64 / secs,
                stat_updates as f64 / secs,
                skipped - stat_skipped0,
                (bytes - stat_bytes0) as f64 * 8.0 / secs / 1e6,
                rc.cur,
                loss * 100.0,
                resent - stat_resent0,
                if stat_frames > 0 { stat_enc.as_secs_f64() * 1000.0 / stat_frames as f64 } else { 0.0 },
                if sh.peer_opus.load(Ordering::Relaxed) { "Opus" } else { "обычный" },
                if glitches > 0 { format!(" · пропуски в захвате звука: {glitches}") } else { String::new() },
            ));
            stat_resent0 = resent;
            stat_updates = 0;
            stat_skipped0 = skipped;
            stat_t = Instant::now();
            stat_frames = 0;
            stat_enc = Duration::ZERO;
            stat_bytes0 = bytes;
        }
    }
    drop(audio);
    drop(pv);
    let _ = pv_thread.join();
    drop(tx);
    let _ = sender.join();
    let mut bye = Vec::new();
    net::write_header(&mut bye, net::PT_BYE, token);
    for _ in 0..3 {
        let _ = sock.send_to(&bye, peer);
    }
    Ok(())
}

/// True when the window capture ended because the window is gone (not because the capture itself broke).
fn cap_window_gone(cap: &Cap) -> bool {
    match cap {
        Cap::Window(w) => !w.alive(),
        Cap::Screen(_) => false,
    }
}
