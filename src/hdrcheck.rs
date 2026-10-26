// `hdrcheck`: end-to-end test of the HDR capture path on a real monitor. A borderless window covers the monitor
// with stripes of known sRGB colours; the stream's own capturer grabs the screen (float16 + tone mapping when
// HDR is on, plain BGRA otherwise) and every stripe is compared with what was drawn. Windows draws an SDR
// window into an HDR desktop at the "SDR content brightness" level, so a correct path gives the same numbers
// back: grey 128 stays 128, white stays white, nothing is washed out or darkened.
use anyhow::{bail, Result};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId};

/// (name, sRGB colour) of every stripe, left to right.
const PATCHES: [(&str, [u8; 3]); 11] = [
    ("чёрный", [0, 0, 0]),
    ("серый 12%", [32, 32, 32]),
    ("серый 25%", [64, 64, 64]),
    ("серый 50%", [128, 128, 128]),
    ("серый 75%", [192, 192, 192]),
    ("серый 90%", [230, 230, 230]),
    ("белый", [255, 255, 255]),
    ("красный", [220, 40, 40]),
    ("зелёный", [40, 200, 70]),
    ("синий", [40, 80, 220]),
    ("кожа", [224, 172, 140]),
];

fn stripe_of(x: u32, w: u32) -> usize {
    ((x as u64 * PATCHES.len() as u64) / w.max(1) as u64) as usize
}

pub struct Row {
    pub name: &'static str,
    pub want: [u8; 3],
    pub got: [u8; 3],
}

/// Shows the stripes on `monitor` and measures them through the capture path. Returns one row per stripe.
pub fn run(monitor: crate::capture::MonitorInfo) -> Result<Vec<Row>> {
    let (tx, rx) = mpsc::channel::<Result<Vec<Row>>>();
    let el = EventLoop::<()>::with_user_event().build()?;
    let proxy = el.create_proxy();
    let (left, top) = (monitor.left, monitor.top);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500)); // let the window appear and settle
        let r = measure(&monitor);
        let _ = tx.send(r);
        let _ = proxy.send_event(());
    });
    struct App {
        window: Option<Rc<Window>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        at: (i32, i32),
        until: Instant,
    }
    impl ApplicationHandler for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            let mon = el.available_monitors().find(|m| m.position().x == self.at.0 && m.position().y == self.at.1).or(el.primary_monitor());
            let attrs = Window::default_attributes().with_title("screenshare — проверка HDR").with_fullscreen(Some(Fullscreen::Borderless(mon)));
            let w = Rc::new(el.create_window(attrs).expect("window"));
            let ctx = softbuffer::Context::new(w.clone()).expect("softbuffer");
            self.surface = Some(softbuffer::Surface::new(&ctx, w.clone()).expect("surface"));
            w.request_redraw();
            self.window = Some(w);
        }
        fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
            el.exit();
        }
        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
            match ev {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::RedrawRequested => {
                    let (Some(w), Some(s)) = (&self.window, &mut self.surface) else { return };
                    let size = w.inner_size();
                    let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
                    let _ = s.resize(ww, wh);
                    if let Ok(mut buf) = s.buffer_mut() {
                        let w = size.width as usize;
                        for (i, px) in buf.iter_mut().enumerate() {
                            let [r, g, b] = PATCHES[stripe_of((i % w) as u32, size.width).min(PATCHES.len() - 1)].1;
                            *px = (r as u32) << 16 | (g as u32) << 8 | b as u32;
                        }
                        let _ = buf.present();
                    }
                }
                _ => {}
            }
        }
        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            if Instant::now() > self.until {
                el.exit();
            }
            el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100)));
        }
    }
    let mut app = App { window: None, surface: None, at: (left, top), until: Instant::now() + Duration::from_secs(20) };
    el.run_app(&mut app)?;
    rx.recv_timeout(Duration::from_secs(1)).unwrap_or_else(|_| bail!("захват не ответил"))
}

fn measure(monitor: &crate::capture::MonitorInfo) -> Result<Vec<Row>> {
    let mut cap = crate::capture::Capturer::new(monitor)?;
    let start = Instant::now();
    let mut got_frame = false;
    while start.elapsed() < Duration::from_secs(3) {
        if let crate::capture::Event::Updated = cap.next(200)? {
            got_frame = true;
        }
        if got_frame && start.elapsed() > Duration::from_millis(600) {
            break;
        }
    }
    let (w, h) = (cap.width, cap.height);
    let stride = w as usize * 4;
    let mut buf = vec![0u8; stride * h as usize];
    cap.compose(&mut buf, stride);
    let mut rows = Vec::new();
    for (i, (name, want)) in PATCHES.iter().enumerate() {
        // Centre of the stripe, a 15x15 block (away from the stripe edges and the mouse, which sits elsewhere).
        let cx = ((2 * i + 1) as u64 * w as u64 / (2 * PATCHES.len()) as u64) as usize;
        let cy = h as usize / 3;
        let mut sum = [0u32; 3];
        let mut n = 0;
        for y in cy - 7..=cy + 7 {
            for x in cx.saturating_sub(7)..=(cx + 7).min(w as usize - 1) {
                let p = &buf[y * stride + x * 4..][..3]; // BGRA
                sum[0] += p[2] as u32;
                sum[1] += p[1] as u32;
                sum[2] += p[0] as u32;
                n += 1;
            }
        }
        let got = [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8];
        rows.push(Row { name, want: *want, got });
    }
    Ok(rows)
}
