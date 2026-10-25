// Client side: UDP -> frame reassembly -> D3D11VA decode -> window. Audio into a jitter buffer.
use crate::net::{self, Code};
use crate::ui::{self, Exchange};
use crate::video::{Decoder, Scaler};
use anyhow::Result;
use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

struct Partial {
    count: u16,
    got: u16,
    key: bool,
    frags: Vec<Option<Vec<u8>>>,
    first: Instant,
}

/// Puts fragments back together and hands out frames strictly in order.
/// After any loss it waits for the next keyframe, since later frames reference the lost one.
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    done: BTreeMap<u32, (Vec<u8>, bool)>,
    next: Option<u32>,
    pub waiting_key: bool,
    pub losses: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Reassembler { partial: BTreeMap::new(), done: BTreeMap::new(), next: None, waiting_key: true, losses: 0 }
    }

    pub fn on_fragment(&mut self, fid: u32, idx: u16, count: u16, key: bool, data: &[u8], out: &mut Vec<(Vec<u8>, bool)>) {
        if self.next.is_some_and(|n| fid < n) || self.done.contains_key(&fid) || count == 0 || idx >= count {
            return;
        }
        let p = self.partial.entry(fid).or_insert_with(|| Partial {
            count,
            got: 0,
            key,
            frags: vec![None; count as usize],
            first: Instant::now(),
        });
        if p.count != count || p.frags[idx as usize].is_some() {
            return;
        }
        p.frags[idx as usize] = Some(data.to_vec());
        p.got += 1;
        if p.got == p.count {
            let p = self.partial.remove(&fid).unwrap();
            let mut buf = Vec::with_capacity(p.count as usize * net::MAX_PAYLOAD);
            for f in p.frags {
                buf.extend_from_slice(&f.unwrap());
            }
            self.done.insert(fid, (buf, p.key));
        }
        // Bound memory if the stream is badly broken.
        while self.partial.len() > 120 {
            self.partial.pop_first();
        }
        self.drain(out);
    }

    /// A frame that never completes (its last packets were lost) must not stall the stream forever.
    pub fn check_timeout(&mut self, out: &mut Vec<(Vec<u8>, bool)>) {
        if self.waiting_key {
            return;
        }
        if let Some(n) = self.next {
            if self.partial.get(&n).is_some_and(|p| p.first.elapsed() > Duration::from_millis(150)) {
                self.lose();
                self.drain(out);
            }
        }
    }

    fn lose(&mut self) {
        self.waiting_key = true;
        self.losses += 1;
    }

    fn drain(&mut self, out: &mut Vec<(Vec<u8>, bool)>) {
        loop {
            if self.waiting_key {
                let Some(k) = self.done.iter().find(|(_, (_, key))| *key).map(|(f, _)| *f) else {
                    // Keep only what could follow a keyframe that is still arriving.
                    match self.partial.iter().find(|(_, p)| p.key).map(|(f, _)| *f) {
                        Some(kp) => self.done.retain(|&f, _| f > kp),
                        None => self.done.clear(),
                    }
                    return;
                };
                self.done = self.done.split_off(&k);
                self.partial = self.partial.split_off(&k);
                self.next = Some(k);
                self.waiting_key = false;
            }
            let n = self.next.unwrap();
            if let Some(f) = self.done.remove(&n) {
                out.push(f);
                self.next = Some(n + 1);
                self.partial = self.partial.split_off(&(n + 1));
                continue;
            }
            // Three newer frames finished while this one is still missing: it's lost.
            if self.done.last_key_value().is_some_and(|(&last, _)| last >= n + 3) {
                self.lose();
                continue;
            }
            return;
        }
    }
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
    losses: AtomicU64,
    /// Window client size, (w << 32) | h.
    win_size: AtomicU64,
    picture: Mutex<Picture>,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

#[derive(Debug)]
enum UserEv {
    Frame,
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
        losses: AtomicU64::new(0),
        win_size: AtomicU64::new((1280u64 << 32) | 720),
        picture: Mutex::new(Picture { w: 0, h: 0, px: Vec::new(), flash: false }),
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

    // Room for the lip-sync hold (up to MAX_SYNC_HOLD of frames) plus slack.
    let (frame_tx, frame_rx) = mpsc::sync_channel::<(Vec<u8>, bool, Instant)>(48);
    let sync_q = audio_q.clone();

    // Receiver: reassembly + audio.
    {
        let sock = sock.clone();
        let sh = sh.clone();
        std::thread::spawn(move || {
            let _ = sock.set_read_timeout(Some(Duration::from_millis(10)));
            let mut buf = [0u8; 2048];
            let mut re = Reassembler::new();
            let mut ready = Vec::new();
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
                                    re.on_fragment(fid, idx, count, body[8] & 1 != 0, &body[9..], &mut ready);
                                }
                                net::PT_AUDIO if body.len() >= 8 => {
                                    if let Some(q) = &audio_q {
                                        let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                                        samples.clear();
                                        samples.extend(body[8..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
                                        q.push(&samples, rate);
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
                    Err(e) if net::is_benign(&e) => {}
                    Err(e) => eprintln!("сеть: {e}"),
                }
                re.check_timeout(&mut ready);
                for (data, key) in ready.drain(..) {
                    if frame_tx.try_send((data, key, Instant::now())).is_err() {
                        // Decoder fell behind: skip ahead to a fresh keyframe.
                        re.waiting_key = true;
                        re.losses += 1;
                    }
                }
                sh.waiting_key.store(re.waiting_key, Ordering::Relaxed);
                sh.losses.store(re.losses, Ordering::Relaxed);
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
            while sh.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                tick += 1;
                if sh.waiting_key.load(Ordering::Relaxed) && tick % 3 == 0 {
                    net::write_header(&mut out, net::PT_KEYREQ, token);
                    let _ = sock.send_to(&out, peer);
                }
                if tick % 5 == 0 {
                    net::write_header(&mut out, net::PT_PING, token);
                    out.extend_from_slice(&(sh.start.elapsed().as_micros() as u64).to_le_bytes());
                    let _ = sock.send_to(&out, peer);
                }
                if sh.now_ms().saturating_sub(sh.last_seen_ms.load(Ordering::Relaxed)) > 10_000 {
                    println!("Связь потеряна.");
                    sh.running.store(false, Ordering::Relaxed);
                }
                if stat_t.elapsed() >= Duration::from_secs(5) {
                    let secs = stat_t.elapsed().as_secs_f64();
                    let b = sh.recv_bytes.load(Ordering::Relaxed);
                    let f = sh.frames_decoded.load(Ordering::Relaxed);
                    ui::stat(&format!(
                        "{:.0} кадров/с · {:.1} Мбит/с · пинг {:.0} мс · потерь {}",
                        (f - f0) as f64 / secs,
                        (b - b0) as f64 * 8.0 / secs / 1e6,
                        sh.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0,
                        sh.losses.load(Ordering::Relaxed),
                    ));
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
            let mut dec = match Decoder::new(hw) {
                Ok(d) => d,
                Err(e) => {
                    println!("Декодер не запустился: {e:#}");
                    sh.running.store(false, Ordering::Relaxed);
                    return;
                }
            };
            println!("Декодирование: {}", if dec.hardware { "видеокарта (D3D11VA)" } else { "процессор" });
            let mut scaler = Scaler::new();
            let mut full = Scaler::new();
            let mut px: Vec<u32> = Vec::new();
            let mut done = false;
            let mut flash = crate::avsync::FlashEdge::default();
            for (data, _key, arrived) in frame_rx {
                // Lip sync: sound sits in its jitter buffer for a while, so hold each frame just as long.
                if let Some(q) = &sync_q {
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
                let r = dec.decode(&data, |f| {
                    let ws = sh.win_size.load(Ordering::Relaxed);
                    let (ww, wh) = ((ws >> 32) as u32, ws as u32);
                    if ww == 0 || wh == 0 || f.width <= 0 || f.height <= 0 {
                        return;
                    }
                    let (fw, fh) = (f.width as u32, f.height as u32);
                    let s = (ww as f64 / fw as f64).min(wh as f64 / fh as f64);
                    let (dw, dh) = (((fw as f64 * s) as u32).max(1), ((fh as f64 * s) as u32).max(1));
                    if scaler.convert(f, dw, dh, &mut px).is_err() {
                        return;
                    }
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
                if let Err(e) = r {
                    eprintln!("декодер: {e:#}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(r: &mut Reassembler, fid: u32, key: bool, n: u16, out: &mut Vec<(Vec<u8>, bool)>) {
        for i in 0..n {
            r.on_fragment(fid, i, n, key, &[fid as u8], out);
        }
    }

    #[test]
    fn in_order_after_key() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        feed(&mut r, 0, false, 2, &mut out); // non-key before any keyframe is useless
        assert!(out.is_empty());
        feed(&mut r, 1, true, 3, &mut out);
        feed(&mut r, 2, false, 2, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0].1);
        assert_eq!(out[0].0, vec![1, 1, 1]);
    }

    #[test]
    fn loss_waits_for_key() {
        let mut r = Reassembler::new();
        let mut out = Vec::new();
        feed(&mut r, 0, true, 1, &mut out);
        r.on_fragment(1, 0, 2, false, &[1], &mut out); // frame 1 never completes
        for f in 2..5 {
            feed(&mut r, f, false, 1, &mut out);
        }
        assert_eq!(out.len(), 1);
        assert!(r.waiting_key);
        feed(&mut r, 5, false, 1, &mut out);
        feed(&mut r, 6, true, 1, &mut out);
        feed(&mut r, 7, false, 1, &mut out);
        assert_eq!(out.len(), 3);
        assert!(out[1].1);
        assert!(!r.waiting_key);
    }
}
