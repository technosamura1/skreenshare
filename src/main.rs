// GUI app: no console window when started by double-click. CLI use attaches to the parent console.
#![windows_subsystem = "windows"]

mod audio;
mod audiopkt;
mod audiosel;
mod bar;
mod cues;
mod avsync;
mod bundle;
mod driverinst;
mod capture;
mod client;
mod gpuconv;
mod gpulogic;
mod hdr;
mod hdrmath;
mod guilogic;
mod glass;
mod gui;
mod icon;
mod inputview;
mod overlay;
mod xinput;
mod host;
mod inputhost;
mod inputlogic;
mod inputproto;
mod loopback;
mod easy;
mod encsel;
mod net;
mod netq;
mod opus;
mod opusaudio;
mod playout;
mod plc;
mod ntfy;
mod preview;
mod ratectl;
mod reassembly;
mod scale;
mod retx;
mod selftest;
mod source;
mod thumbs;
mod ui;
mod video;
mod viewperf;
mod volume;
mod wincap;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use ui::Exchange;

const HELP: &str = "\
screenshare — показ экрана напрямую, без серверов.

Без параметров — окно программы. Параметры (для продвинутых):
  screenshare host   [--monitor N] [--window HWND] [--bitrate МБИТ] [--fps N] [--preset p1..p7]
                     [--max-send МБИТ] [--no-audio] [--audio-all] [--keep-voice] [--auto-bitrate] [--exchange-dir ПАПКА]
                     [--control] [--no-control-pad] [--no-control-kbm]
                     --bitrate по умолчанию 30 (окно программы даёт «Авто», 30 «Стандарт» и 60 «Высокое качество»), --max-send 100.
                     --auto-bitrate — «Авто»: старт с 20 Мбит/с и подъём до --bitrate (по умолчанию 60), пока нет потерь.
                     Звук окна: при показе окна берётся звук только его процесса; --audio-all — всегда весь звук,
                     --keep-voice — не вырезать Discord. В stdin во время показа: «AUDIO window=0|1 voice=0|1».
                     Удалённое управление: --control разрешает зрителю геймпад и мышь с клавиатурой (по умолчанию выключено),
                     --no-control-pad / --no-control-kbm исключают одно из двух; строка «CONTROL on=0|1 pad=0|1 kbm=0|1» в stdin меняет
                     разрешения на лету. Аварийное отключение — Ctrl+Alt+Shift+F12 на компьютере показывающего.
                     Во время показа окно программы может сменить источник: строка «SOURCE monitor N» / «SOURCE window HWND» в stdin.
  screenshare client [--no-audio] [--no-control] [--cpu-decode] [--cpu-convert] [--windowed] [--exchange-dir ПАПКА]
                     [--frames N --screenshot ФАЙЛ.bmp]
                     --cpu-convert (или переменная SS_CPU_CONVERT=1) — цвет и масштаб на процессоре, не на видеокарте
  screenshare gputest        проверка видеокарты для цвета и масштаба у зрителя (без сети)
  screenshare monitors
  screenshare windows        список окон (номер HWND для --window); --window показывает только это окно
  screenshare audiotest      проверка, что звук Discord не попадает в трансляцию
  screenshare selftest [--monitor N] [--bitrate МБИТ] [--preset pN] [--cpu-decode] [--out ФАЙЛ.bmp]
";

struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        if let Some(i) = self.0.iter().position(|a| a == name) {
            self.0.remove(i);
            true
        } else {
            false
        }
    }
    fn value(&mut self, name: &str) -> Result<Option<String>> {
        let Some(i) = self.0.iter().position(|a| a == name) else { return Ok(None) };
        if i + 1 >= self.0.len() {
            bail!("после {name} нужно значение");
        }
        let v = self.0.remove(i + 1);
        self.0.remove(i);
        Ok(Some(v))
    }
    fn num<T: std::str::FromStr>(&mut self, name: &str, default: T) -> Result<T> {
        match self.value(name)? {
            Some(v) => v.parse().ok().with_context(|| format!("{name}: не число: {v}")),
            None => Ok(default),
        }
    }
    fn finish(&self) -> Result<()> {
        if let Some(a) = self.0.first() {
            bail!("непонятный параметр: {a}\n\n{HELP}");
        }
        Ok(())
    }
}

fn setup_console() {
    unsafe {
        // Real pixel sizes in the monitor list instead of DPI-scaled ones.
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
        let _ = windows::Win32::System::Console::SetConsoleCP(65001);
        // 1 ms timer resolution for frame pacing and the packet pacer.
        windows::Win32::Media::timeBeginPeriod(1);
    }
}

fn exchange(a: &mut Args) -> Result<Exchange> {
    let room = a.value("--room")?;
    Ok(match a.value("--exchange-dir")? {
        Some(d) => Exchange::Dir(PathBuf::from(d)),
        None if room.is_some() => {
            let r = easy::Room::from_code(room.as_deref().unwrap_or(""))
                .context("короткий код неправильный — это 8 символов вроде K7QM-X2PD")?;
            Exchange::Relay(std::sync::Arc::new(r))
        }
        None if ui::ipc() => Exchange::Ipc,
        None => Exchange::Console,
    })
}

fn host_opts(a: &mut Args) -> Result<host::HostOpts> {
    // Авто: --auto-bitrate; --bitrate is then the ceiling of the ramp (60 unless given).
    let auto_bitrate = a.flag("--auto-bitrate");
    let bitrate_mbps = a.num("--bitrate", if auto_bitrate { 60 } else { 30 })?;
    Ok(host::HostOpts {
        window: a.value("--window")?.map(|v| v.parse::<isize>()).transpose().context("--window")?,
        monitor: a.value("--monitor")?.map(|v| v.parse::<usize>()).transpose().context("--monitor")?,
        auto_bitrate,
        bitrate_mbps,
        fps: a.num("--fps", 60)?,
        preset: a.value("--preset")?.unwrap_or_else(|| "p4".into()),
        max_send_mbps: a.num("--max-send", 100)?,
        audio: !a.flag("--no-audio"),
        audio_prefs: audiosel::AudioPrefs { window_only: !a.flag("--audio-all"), exclude_voice: !a.flag("--keep-voice") },
        exchange: exchange(a)?,
        control: {
            let on = a.flag("--control");
            let (no_pad, no_kbm) = (a.flag("--no-control-pad"), a.flag("--no-control-kbm"));
            inputlogic::Perms { allowed: on, pad: !no_pad, kbm: !no_kbm }
        },
    })
}

fn client_opts(a: &mut Args) -> Result<client::ClientOpts> {
    Ok(client::ClientOpts {
        control: !a.flag("--no-control"),
        audio: !a.flag("--no-audio"),
        hw_decode: !a.flag("--cpu-decode"),
        cpu_convert: a.flag("--cpu-convert"),
        windowed: a.flag("--windowed"),
        screenshot: a.value("--screenshot")?.map(PathBuf::from),
        frames: a.value("--frames")?.map(|v| v.parse()).transpose().context("--frames")?,
        exchange: exchange(a)?,
    })
}

fn run(mut a: Args) -> Result<()> {
    let cmd = a.0.remove(0);
    match cmd.as_str() {
        "host" => {
            let o = host_opts(&mut a)?;
            a.finish()?;
            host::run(o)?;
        }
        "client" => {
            let o = client_opts(&mut a)?;
            a.finish()?;
            client::run(o)?;
        }
        "monitors" => {
            a.finish()?;
            for (i, m) in capture::list_monitors()?.iter().enumerate() {
                println!("{i}: {}x{} {}{}", m.width, m.height, m.adapter, if m.primary { " (основной)" } else { "" });
            }
        }
        "windows" => {
            a.finish()?;
            for w in wincap::list_windows() {
                println!("{}: {}{}", w.hwnd, source::ellipsize(&w.title, 80), if w.minimized { " (свёрнуто)" } else { "" });
            }
        }
        "selftest" => {
            let mon = a.value("--monitor")?.map(|v| v.parse::<usize>()).transpose().context("--monitor")?;
            let bitrate = a.num::<u64>("--bitrate", 20)? * 1_000_000;
            let preset = a.value("--preset")?.unwrap_or_else(|| "p4".into());
            let hw = !a.flag("--cpu-decode");
            let out = PathBuf::from(a.value("--out")?.unwrap_or_else(|| "selftest.bmp".into()));
            a.finish()?;
            selftest::color_test(bitrate, &preset, hw)?;
            selftest::bitrate_switch_test(&preset)?;
            let m = host::pick_monitor(mon, false)?;
            selftest::capture_test(m, bitrate, &preset, hw, &out)?;
        }
        "convbench" => {
            let w = a.num::<i32>("--video-w", 1920)?;
            let h = a.num::<i32>("--video-h", 1080)?;
            let dw = a.num::<u32>("--window-w", 2560)?;
            let dh = a.num::<u32>("--window-h", 1440)?;
            a.finish()?;
            video::convert_bench(w, h, dw, dh)?;
        }
        "gputest" => {
            let w = a.num::<i32>("--video-w", 1920)?;
            let h = a.num::<i32>("--video-h", 1080)?;
            let dw = a.num::<u32>("--window-w", 1600)?;
            let dh = a.num::<u32>("--window-h", 900)?;
            a.finish()?;
            gpuconv::selftest(w, h, dw, dh)?;
        }
        "hdrtest" => {
            a.finish()?;
            hdr::selftest()?;
        }
        "overlaytest" => {
            let out = PathBuf::from(a.value("--out")?.unwrap_or_else(|| "overlay.bmp".into()));
            let (panel, on) = (a.flag("--panel"), a.flag("--on"));
            a.finish()?;
            client::overlay_preview(&out, panel, on)?;
        }
        // Hidden: unpack the bundled ViGEmBus setup and run it the way the GUI button does (UAC, silent switches).
        "drvinstall" => {
            a.finish()?;
            let end = driverinst::install(inputhost::vigem_installed)?;
            println!("{}", guilogic::driver_end_text(end));
        }
        "opustest" => {
            a.finish()?;
            opus::selftest()?;
        }
        "avbeacon" => {
            let secs = a.num::<u64>("--seconds", 20)?;
            a.finish()?;
            avsync::run_beacon(secs)?;
        }
        "audiotest" => {
            a.finish()?;
            match loopback::find_voice_app() {
                Some((pid, name)) => println!("Голосовой чат: {name} (PID {pid}) — будет исключён из звука"),
                None => println!("Голосовой чат не запущен"),
            }
            println!("Проверяю исключение программы из захвата (тихий тон 440 Гц ~3 секунды)...");
            let (excluded, included) = loopback::self_check()?;
            println!("тон, когда программа исключена: {excluded:.5}");
            println!("тон, когда программа включена:  {included:.5}");
            if included > 0.002 && excluded < included / 20.0 {
                println!("OK: исключённая программа в трансляцию не попадает");
            } else {
                bail!("исключение не сработало");
            }
        }
        "-h" | "--help" | "help" => print!("{HELP}"),
        other => bail!("непонятная команда: {other}\n\n{HELP}"),
    }
    Ok(())
}

fn main() {
    let mut a = Args(std::env::args().skip(1).collect());
    if a.0.is_empty() {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        // Unpack FFmpeg now, so the stream processes we start later find it ready.
        std::thread::spawn(|| {
            let _ = bundle::ensure_ffmpeg();
        });
        if let Err(e) = gui::run() {
            eprintln!("{e}");
        }
        return;
    }
    if a.flag("--ipc") {
        ui::set_ipc();
    } else {
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
        }
    }
    setup_console();
    if let Err(e) = bundle::ensure_ffmpeg() {
        let msg = format!("не удалось подготовить FFmpeg: {e:#}");
        if ui::ipc() {
            println!("@ERROR {msg}");
        } else {
            println!("Ошибка: {msg}");
        }
        std::process::exit(1);
    }
    if let Err(e) = ffmpeg_next::init() {
        eprintln!("FFmpeg не загрузился: {e}");
    }
    video::set_log_quiet();
    if let Err(e) = run(a) {
        if ui::ipc() {
            println!("@ERROR {}", format!("{e:#}").replace('\n', " "));
        } else {
            println!("Ошибка: {e:#}");
        }
        std::process::exit(1);
    }
}
