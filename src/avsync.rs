// Audio/video sync measurement (test only, enabled by the SS_AVSYNC env var).
// A beacon window flashes white and beeps once a second; host and client log the wall-clock time at which
// they see each flash and hear each beep. Comparing the audio-video offset on the host (at capture) with
// the one on the client (at the screen / speakers) gives the offset our pipeline adds.
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SS_AVSYNC").is_some())
}

/// Wall clock in microseconds: comparable across processes on one PC.
pub fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as i64
}

pub fn log(kind: &str, t_us: i64) {
    println!("@AV {kind} {t_us}");
}

/// Mean brightness (0..255) of a sparse grid over a BGRA / 0RGB image.
pub fn luma(px: &[u8], stride: usize, w: usize, h: usize) -> f32 {
    let mut sum = 0u64;
    let mut n = 0u64;
    for gy in 0..24 {
        let y = (gy * 2 + 1) * h / 48;
        for gx in 0..24 {
            let x = (gx * 2 + 1) * w / 48;
            let p = &px[y * stride + x * 4..][..3];
            sum += (p[0] as u64 + p[1] as u64 + p[2] as u64) / 3;
            n += 1;
        }
    }
    sum as f32 / n.max(1) as f32
}

/// Dark -> bright transitions.
#[derive(Default)]
pub struct FlashEdge(bool);

impl FlashEdge {
    pub fn rising(&mut self, luma: f32) -> bool {
        let bright = if self.0 { luma > 100.0 } else { luma > 160.0 };
        let r = bright && !self.0;
        self.0 = bright;
        r
    }
}

/// Beep onsets: first loud sample after at least 300 ms of near-silence.
pub struct BeepEdge {
    quiet: u32,
    rate: u32,
}

impl BeepEdge {
    pub fn new(rate: u32) -> Self {
        BeepEdge { quiet: 0, rate }
    }

    /// Feed one mono sample (-1..1); true at an onset.
    pub fn feed(&mut self, v: f32) -> bool {
        if v.abs() > 0.05 {
            let onset = self.quiet > self.rate * 3 / 10;
            self.quiet = 0;
            onset
        } else {
            self.quiet = self.quiet.saturating_add(1);
            false
        }
    }
}

const PERIOD_MS: i64 = 1000;
const ON_MS: i64 = 250;

fn beacon_on(t_us: i64) -> bool {
    (t_us / 1000).rem_euclid(PERIOD_MS) < ON_MS
}

/// Fullscreen window on a non-primary monitor that flashes and beeps in step with the wall clock.
pub fn run_beacon(seconds: u64) -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::num::NonZeroU32;
    use std::rc::Rc;
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::window::{Fullscreen, Window, WindowId};

    let dev = cpal::default_host().default_output_device().ok_or_else(|| anyhow::anyhow!("нет звука"))?;
    let cfg = dev.default_output_config()?;
    let ch = cfg.channels() as usize;
    let sr = cfg.sample_rate() as f64;
    let mut phase = 0.0f64;
    let stream = dev.build_output_stream::<f32, _, _>(
        cfg.config(),
        move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let lead = ts.playback.duration_since(ts.callback).as_micros() as i64;
            let t0 = now_us() + lead;
            for (k, f) in out.chunks_mut(ch).enumerate() {
                let t = t0 + (k as f64 * 1e6 / sr) as i64;
                let v = if beacon_on(t) { (phase * std::f64::consts::TAU).sin() as f32 * 0.15 } else { 0.0 };
                phase = (phase + 1000.0 / sr).fract();
                f.fill(v);
            }
        },
        |e| eprintln!("маяк: {e}"),
        None,
    )?;
    stream.play()?;

    struct Beacon {
        window: Option<Rc<Window>>,
        surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
        until: std::time::Instant,
    }
    impl ApplicationHandler for Beacon {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            let primary = el.primary_monitor();
            let mon = el.available_monitors().find(|m| Some(m) != primary.as_ref()).or(primary);
            let attrs = Window::default_attributes()
                .with_title("screenshare — маяк синхронизации")
                .with_fullscreen(Some(Fullscreen::Borderless(mon)));
            let w = Rc::new(el.create_window(attrs).unwrap());
            let ctx = softbuffer::Context::new(w.clone()).unwrap();
            self.surface = Some(softbuffer::Surface::new(&ctx, w.clone()).unwrap());
            self.window = Some(w);
        }
        fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
            match ev {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::RedrawRequested => {
                    let (Some(w), Some(s)) = (&self.window, &mut self.surface) else { return };
                    let size = w.inner_size();
                    if let (Some(ww), Some(wh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                        let _ = s.resize(ww, wh);
                        if let Ok(mut buf) = s.buffer_mut() {
                            buf.fill(if beacon_on(now_us()) { 0x00FF_FFFF } else { 0 });
                            let _ = buf.present();
                        }
                    }
                }
                _ => {}
            }
        }
        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            if std::time::Instant::now() > self.until {
                el.exit();
                return;
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            el.set_control_flow(ControlFlow::WaitUntil(std::time::Instant::now() + Duration::from_millis(2)));
        }
    }
    let el = EventLoop::new()?;
    el.run_app(&mut Beacon { window: None, surface: None, until: std::time::Instant::now() + Duration::from_secs(seconds) })?;
    drop(stream);
    Ok(())
}
