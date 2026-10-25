// GUI app: no console window when started by double-click. CLI use attaches to the parent console.
#![windows_subsystem = "windows"]

mod audio;
mod avsync;
mod bundle;
mod capture;
mod client;
mod gui;
mod host;
mod loopback;
mod net;
mod selftest;
mod ui;
mod video;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use ui::Exchange;

const HELP: &str = "\
screenshare — показ экрана другу напрямую, без серверов.

Без параметров — окно программы. Параметры (для продвинутых):
  screenshare host   [--monitor N] [--bitrate МБИТ] [--fps N] [--preset p1..p7]
                     [--max-send МБИТ] [--no-audio] [--exchange-dir ПАПКА]
  screenshare client [--no-audio] [--cpu-decode] [--windowed] [--exchange-dir ПАПКА]
                     [--frames N --screenshot ФАЙЛ.bmp]
  screenshare monitors
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
    Ok(match a.value("--exchange-dir")? {
        Some(d) => Exchange::Dir(PathBuf::from(d)),
        None if ui::ipc() => Exchange::Ipc,
        None => Exchange::Console,
    })
}

fn host_opts(a: &mut Args) -> Result<host::HostOpts> {
    Ok(host::HostOpts {
        monitor: a.value("--monitor")?.map(|v| v.parse::<usize>()).transpose().context("--monitor")?,
        bitrate_mbps: a.num("--bitrate", 40)?,
        fps: a.num("--fps", 60)?,
        preset: a.value("--preset")?.unwrap_or_else(|| "p4".into()),
        max_send_mbps: a.num("--max-send", 80)?,
        audio: !a.flag("--no-audio"),
        exchange: exchange(a)?,
    })
}

fn client_opts(a: &mut Args) -> Result<client::ClientOpts> {
    Ok(client::ClientOpts {
        audio: !a.flag("--no-audio"),
        hw_decode: !a.flag("--cpu-decode"),
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
        "selftest" => {
            let mon = a.value("--monitor")?.map(|v| v.parse::<usize>()).transpose().context("--monitor")?;
            let bitrate = a.num::<u64>("--bitrate", 40)? * 1_000_000;
            let preset = a.value("--preset")?.unwrap_or_else(|| "p4".into());
            let hw = !a.flag("--cpu-decode");
            let out = PathBuf::from(a.value("--out")?.unwrap_or_else(|| "selftest.bmp".into()));
            a.finish()?;
            selftest::color_test(bitrate, &preset, hw)?;
            selftest::bitrate_switch_test(&preset)?;
            let m = host::pick_monitor(mon, false)?;
            selftest::capture_test(m, bitrate, &preset, hw, &out)?;
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
